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
