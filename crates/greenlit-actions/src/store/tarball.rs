//! Fetching an action's source by downloading GitHub's repository tarball.
//!
//! `PHASE-3-actions.md`: "Fetch action source (tarball download… ) into a
//! content-addressed action store." Uses the documented endpoint
//! `GET /repos/{owner}/{repo}/tarball/{ref}` ("Gets a redirect URL to
//! download a tar archive for a repository", 302 to a time-limited
//! download URL —
//! <https://docs.github.com/en/rest/repos/contents#download-a-repository-archive-tar>),
//! requested with the already-resolved commit SHA as `{ref}` so the
//! downloaded content is exactly the pinned commit.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tracing::Instrument;

use crate::sha::CommitSha;
use crate::stage_span;
use crate::store::extract::extract_tarball;
use crate::store::fetcher::{ActionFetcher, FetchError};

/// Generous relative to a repository tarball transfer; bounds a stalled
/// connection rather than reflecting an expected transfer time.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// A ceiling on the compressed transfer itself, ahead of (and smaller than)
/// [`crate::store::extract::MAX_EXTRACTED_BYTES`] which bounds the
/// *decompressed* content — gzip amplification means the compressed and
/// uncompressed bounds are deliberately different sizes.
const MAX_COMPRESSED_BYTES: u64 = 128 * 1024 * 1024;
const USER_AGENT: &str = concat!("greenlit-litci/", env!("CARGO_PKG_VERSION"));
const API_VERSION: &str = "2022-11-28";

/// Downloads and safely extracts a repository tarball for one pinned
/// commit.
#[derive(Debug, Clone)]
pub struct TarballFetcher {
    agent: ureq::Agent,
    base_url: String,
    token: Option<String>,
}

impl TarballFetcher {
    /// Downloads anonymously from real GitHub — works for public
    /// repositories, subject to GitHub's unauthenticated rate limit.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(None, "https://api.github.com")
    }

    /// Downloads from real GitHub using `token` — required for private
    /// repositories, and raises the rate limit for public ones.
    #[must_use]
    pub fn with_token(token: impl Into<String>) -> Self {
        Self::with_base_url(Some(token.into()), "https://api.github.com")
    }

    /// A fetcher against a caller-chosen base URL, for tests (see
    /// [`crate::resolve::GitHubApiResolver::with_base_url`] for why a real
    /// local HTTP server is the correct injection point rather than a fake
    /// [`ActionFetcher`]).
    #[must_use]
    pub fn with_base_url(token: Option<String>, base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
            base_url: base_url.into(),
            token,
        }
    }
}

impl Default for TarballFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ActionFetcher for TarballFetcher {
    async fn fetch(
        &self,
        owner: &str,
        repo: &str,
        sha: &CommitSha,
        dest: &Path,
    ) -> Result<(), FetchError> {
        let agent = self.agent.clone();
        let url = format!("{}/repos/{owner}/{repo}/tarball/{sha}", self.base_url);
        let token = self.token.clone();
        let dest = dest.to_path_buf();
        let owner_o = owner.to_owned();
        let repo_o = repo.to_owned();
        let sha_o = sha.as_str().to_owned();
        let span = stage_span("action-fetch");
        async move {
            tokio::task::spawn_blocking(move || {
                fetch_blocking(
                    &agent,
                    &url,
                    token.as_deref(),
                    &dest,
                    &owner_o,
                    &repo_o,
                    &sha_o,
                )
            })
            .await
            .map_err(|join_error| FetchError::Download {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                sha: sha.as_str().to_owned(),
                message: format!("fetch task did not complete: {join_error}"),
            })?
        }
        .instrument(span)
        .await
    }
}

fn fetch_blocking(
    agent: &ureq::Agent,
    url: &str,
    token: Option<&str>,
    dest: &Path,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Result<(), FetchError> {
    let download_error = |message: String| FetchError::Download {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        sha: sha.to_owned(),
        message,
    };

    let mut request = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", USER_AGENT)
        .header("X-GitHub-Api-Version", API_VERSION);
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let mut response = request.call().map_err(|error| {
        if matches!(error, ureq::Error::Timeout(_)) {
            FetchError::TimedOut {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                sha: sha.to_owned(),
                seconds: DOWNLOAD_TIMEOUT.as_secs(),
            }
        } else {
            download_error(error.to_string())
        }
    })?;

    let status = response.status();
    if !status.is_success() {
        let mut body = response
            .body_mut()
            .with_config()
            .limit(4096)
            .read_to_vec()
            .unwrap_or_default();
        body.truncate(4096.min(body.len()));
        return Err(download_error(format!(
            "HTTP {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(&body).trim()
        )));
    }

    let body_reader = response
        .body_mut()
        .with_config()
        .limit(MAX_COMPRESSED_BYTES)
        .reader();
    let gzip = flate2::read::GzDecoder::new(body_reader);
    extract_into(gzip, dest, owner, repo, sha)
}

fn extract_into(
    reader: impl Read,
    dest: &Path,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Result<(), FetchError> {
    extract_tarball(reader, dest).map_err(|error| FetchError::Extract {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        sha: sha.to_owned(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};

    fn gzip_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *content).unwrap();
        }
        let tar_bytes = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn respond_with_tarball(listener: TcpListener, body: Vec<u8>) {
        let (mut stream, _) = listener.accept().expect("accept");
        drain_request(&mut stream);
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
    }

    fn drain_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0_u8; 8192];
        let mut total = Vec::new();
        loop {
            let read = stream.read(&mut buf).unwrap_or(0);
            if read == 0 {
                break;
            }
            total.extend_from_slice(&buf[..read]);
            if total.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
    }

    #[tokio::test]
    async fn downloads_and_extracts_a_tarball() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let body = gzip_tarball(&[
            ("repo-sha/action.yml", b"name: test"),
            ("repo-sha/main.js", b"console.log(1)"),
        ]);
        let handle = std::thread::spawn(move || respond_with_tarball(listener, body));

        let fetcher = TarballFetcher::with_base_url(None, base_url);
        let dest = tempfile::tempdir().unwrap();
        let sha = CommitSha::parse(&"a".repeat(40)).unwrap();
        fetcher
            .fetch("owner", "repo", &sha, dest.path())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.path().join("action.yml")).unwrap(),
            "name: test"
        );
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn non_success_status_is_a_download_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            drain_request(&mut stream);
            let body = r#"{"message":"Not Found"}"#;
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let fetcher = TarballFetcher::with_base_url(None, base_url);
        let dest = tempfile::tempdir().unwrap();
        let sha = CommitSha::parse(&"a".repeat(40)).unwrap();
        let error = fetcher
            .fetch("owner", "repo", &sha, dest.path())
            .await
            .unwrap_err();
        assert!(matches!(error, FetchError::Download { .. }));
        handle.join().unwrap();
    }
}
