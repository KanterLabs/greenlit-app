//! Authenticated ref resolution via the GitHub REST API.
//!
//! `PHASE-3-actions.md`: "Resolve refs … to a commit SHA via the GitHub API
//! when a token exists." Uses the documented "Get a commit" endpoint,
//! `GET /repos/{owner}/{repo}/commits/{ref}`, whose `ref` parameter accepts
//! "a commit SHA, branch name (`heads/BRANCH_NAME`), or tag name
//! (`tags/TAG_NAME`)"
//! (<https://docs.github.com/en/rest/commits/commits#get-a-commit>) — the
//! same branch-vs-tag ambiguity `git_ls_remote` resolves client-side is
//! instead resolved server-side by GitHub itself here, which is the more
//! faithful of the two by construction (`greenlit-v0-spec.md` "Fidelity":
//! "match GitHub's documented behavior").

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::Instrument;

use crate::resolve::{RefResolver, ResolveError};
use crate::sha::CommitSha;
use crate::stage_span;

/// Generous relative to a typical REST round trip; bounds a stalled
/// connection rather than reflecting an expected latency.
const API_TIMEOUT: Duration = Duration::from_secs(20);
/// The commit-lookup response is small JSON; this bounds a pathological or
/// compromised response rather than reflecting a realistic size (a commit
/// with an enormous file list could otherwise grow unboundedly).
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
/// GitHub's REST API requires a `User-Agent` header on every request and
/// documents rejecting requests that omit one.
const USER_AGENT: &str = concat!("greenlit-litci/", env!("CARGO_PKG_VERSION"));
/// Pinned REST API version header, per GitHub's versioning documentation.
const API_VERSION: &str = "2022-11-28";

/// Resolves refs by asking the GitHub REST API directly, using a caller
/// supplied token.
#[derive(Debug, Clone)]
pub struct GitHubApiResolver {
    agent: ureq::Agent,
    base_url: String,
    token: String,
}

impl GitHubApiResolver {
    /// A resolver against the real `https://api.github.com`.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url(token, "https://api.github.com")
    }

    /// A resolver against a caller-chosen base URL — for tests, a local
    /// loopback HTTP server standing in for the GitHub API (`TESTING.md`:
    /// "Behavior tests that inject a true external boundary such as … the
    /// GitHub API … belong here"; a real HTTP client speaking real HTTP to a
    /// real, if local and minimal, server is that injection, not a mock of
    /// this crate's own code).
    #[must_use]
    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(API_TIMEOUT))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
            base_url: base_url.into(),
            token: token.into(),
        }
    }
}

#[async_trait]
impl RefResolver for GitHubApiResolver {
    async fn resolve(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<CommitSha, ResolveError> {
        let agent = self.agent.clone();
        let url = format!("{}/repos/{owner}/{repo}/commits/{git_ref}", self.base_url);
        let token = self.token.clone();
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        let git_ref = git_ref.to_owned();
        let span = stage_span("action-resolve");
        async move {
            let (o, r, g) = (owner.clone(), repo.clone(), git_ref.clone());
            tokio::task::spawn_blocking(move || resolve_blocking(&agent, &url, &token, &o, &r, &g))
                .await
                .map_err(|join_error| ResolveError::TaskFailed {
                    owner,
                    repo,
                    git_ref,
                    message: join_error.to_string(),
                })?
        }
        .instrument(span)
        .await
    }
}

#[derive(Debug, Deserialize)]
struct CommitResponse {
    sha: String,
}

fn resolve_blocking(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    owner: &str,
    repo: &str,
    git_ref: &str,
) -> Result<CommitSha, ResolveError> {
    let api_error = |message: String| ResolveError::Api {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        git_ref: git_ref.to_owned(),
        message,
    };

    let mut response = agent
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", USER_AGENT)
        .header("X-GitHub-Api-Version", API_VERSION)
        .call()
        .map_err(|error| classify_transport_error(error, owner, repo, git_ref))?;

    let status = response.status();
    let mut body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|error| api_error(format!("could not read response body: {error}")))?;
    // Never retain more of a remote's response than needed to report it.
    body.truncate(4096.min(body.len()));

    if status.as_u16() == 404 {
        return Err(ResolveError::NotFound {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            git_ref: git_ref.to_owned(),
        });
    }
    if !status.is_success() {
        return Err(api_error(format!(
            "HTTP {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(&body).trim()
        )));
    }

    let parsed: CommitResponse = serde_json::from_slice(&body)
        .map_err(|error| api_error(format!("could not parse commit response: {error}")))?;
    CommitSha::parse(&parsed.sha)
        .map_err(|error| api_error(format!("GitHub returned an unexpected commit sha: {error}")))
}

fn classify_transport_error(
    error: ureq::Error,
    owner: &str,
    repo: &str,
    git_ref: &str,
) -> ResolveError {
    if matches!(error, ureq::Error::Timeout(_)) {
        return ResolveError::TimedOut {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            git_ref: git_ref.to_owned(),
            seconds: API_TIMEOUT.as_secs(),
        };
    }
    ResolveError::Api {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        git_ref: git_ref.to_owned(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// A minimal single-request HTTP/1.1 server bound to loopback, standing
    /// in for `api.github.com` so `GitHubApiResolver`'s real HTTP-request
    /// construction and response handling run against a real (if tiny and
    /// local) server rather than a fake `RefResolver` — see the type's doc
    /// comment for why this is the correct boundary to exercise here.
    struct FakeGitHub {
        listener: TcpListener,
    }

    impl FakeGitHub {
        fn bind() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            FakeGitHub { listener }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.listener.local_addr().unwrap())
        }

        /// Accepts exactly one connection, drains the request, and writes
        /// back a fixed status/body.
        fn respond_once(self, status: u16, reason: &'static str, body: &'static str) {
            let (mut stream, _) = self.listener.accept().expect("accept");
            drain_request(&mut stream);
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
    }

    fn drain_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
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
    async fn resolves_a_commit_sha_from_a_successful_response() {
        let server = FakeGitHub::bind();
        let base_url = server.base_url();
        let sha = "a".repeat(40);
        let body = format!(r#"{{"sha":"{sha}"}}"#);
        let handle = std::thread::spawn(move || {
            server.respond_once(200, "OK", Box::leak(body.into_boxed_str()))
        });

        let resolver = GitHubApiResolver::with_base_url("test-token", base_url);
        let resolved = resolver.resolve("owner", "repo", "main").await.unwrap();
        assert_eq!(resolved.as_str(), sha);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn maps_404_to_not_found() {
        let server = FakeGitHub::bind();
        let base_url = server.base_url();
        let handle = std::thread::spawn(move || {
            server.respond_once(404, "Not Found", r#"{"message":"Not Found"}"#)
        });

        let resolver = GitHubApiResolver::with_base_url("test-token", base_url);
        let error = resolver
            .resolve("owner", "repo", "missing")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            ResolveError::NotFound {
                owner: "owner".into(),
                repo: "repo".into(),
                git_ref: "missing".into(),
            }
        );
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn maps_other_error_statuses_to_api_error() {
        let server = FakeGitHub::bind();
        let base_url = server.base_url();
        let handle = std::thread::spawn(move || {
            server.respond_once(403, "Forbidden", r#"{"message":"rate limited"}"#)
        });

        let resolver = GitHubApiResolver::with_base_url("test-token", base_url);
        let error = resolver.resolve("owner", "repo", "main").await.unwrap_err();
        match error {
            ResolveError::Api { message, .. } => {
                assert!(message.contains("403"), "message was: {message}");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn user_agent_is_non_empty() {
        assert!(!USER_AGENT.is_empty());
    }
}
