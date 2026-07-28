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
