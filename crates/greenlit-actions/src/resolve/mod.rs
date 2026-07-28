//! Ref → commit SHA resolution.
//!
//! `PHASE-3-actions.md`: "Resolve refs (tag, branch, SHA) to a commit SHA
//! via the GitHub API when a token exists, or via `git ls-remote`
//! tokenless." The network/API surface is the [`RefResolver`] trait so
//! `greenlit-engine`/`greenlit-runtime` can drive real resolution
//! ([`github_api::GitHubApiResolver`], [`git_ls_remote::GitLsRemoteResolver`]).
//! Tests exercise those true boundaries through loopback HTTP and local Git.
//!
//! Callers do not need to pick a resolver themselves for the one case that
//! never touches the network at all: [`resolve_ref`] checks
//! [`CommitSha::looks_like_sha`] first and returns immediately when the
//! authored ref is already a full SHA, matching `PHASE-3-actions.md`'s "A
//! ref that is already a full 40-hex SHA resolves to itself without
//! network." Which concrete [`RefResolver`] to construct for the non-SHA
//! case (API when a token is available, `git ls-remote` otherwise) is a
//! decision for the caller wiring this crate in, since only the caller knows
//! whether a token is available.

mod git_ls_remote;
mod github_api;
mod persistent;
mod pinned;

pub use git_ls_remote::GitLsRemoteResolver;
pub use github_api::GitHubApiResolver;
pub use persistent::PersistentRefResolver;
pub use pinned::PinnedRefResolver;

use async_trait::async_trait;

use crate::sha::CommitSha;

/// The injected boundary that turns a ref into a commit SHA against a real
/// GitHub-hosted repository.
///
/// Implementations perform real I/O through an HTTP request or spawned `git`.
#[async_trait]
pub trait RefResolver: Send + Sync {
    /// Resolves `git_ref` (a tag or branch name — never called for a ref
    /// that already looks like a full SHA; see [`resolve_ref`]) against
    /// `owner/repo`, returning the commit it points at.
    ///
    /// # Errors
    /// Returns [`ResolveError`] when the ref does not exist, the repository
    /// is inaccessible, or the underlying I/O failed.
    async fn resolve(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<CommitSha, ResolveError>;
}

/// A ref could not be resolved to a commit SHA.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// Neither a branch, tag, nor commit named `git_ref` exists in
    /// `owner/repo` (or the repository itself does not exist / is not
    /// visible to the credentials in use — GitHub's API and unauthenticated
    /// `git` both report a missing ref and a missing/private repository
    /// identically, as a not-found response, so this crate cannot
    /// distinguish the two).
    #[error("'{git_ref}' does not resolve to a branch, tag, or commit in {owner}/{repo}")]
    NotFound {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The ref that failed to resolve.
        git_ref: String,
    },
    /// The GitHub REST API rejected the request for a reason other than
    /// "not found" (rate limit, malformed/expired token, server error).
    #[error("GitHub API request for {owner}/{repo}@{git_ref} failed: {message}")]
    Api {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The ref being resolved.
        git_ref: String,
        /// A bounded, already-safe-to-display diagnostic (status code and/or
        /// response body excerpt).
        message: String,
    },
    /// `git` (for `git ls-remote`) could not even be spawned, or its
    /// process I/O failed, independent of what the remote said.
    #[error("could not run 'git {args}': {message}")]
    CommandFailed {
        /// The subcommand and arguments that were attempted, space-joined.
        args: String,
        /// The underlying I/O error's message.
        message: String,
    },
    /// A local/network Git or HTTP operation did not complete within its
    /// fixed deadline.
    #[error("resolving {owner}/{repo}@{git_ref} exceeded the {seconds}-second network deadline")]
    TimedOut {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The ref being resolved.
        git_ref: String,
        /// The deadline in seconds.
        seconds: u64,
    },
    /// The background task performing blocking resolution work panicked or
    /// was cancelled before it could return a result.
    #[error("the resolver task for {owner}/{repo}@{git_ref} did not complete: {message}")]
    TaskFailed {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The ref being resolved.
        git_ref: String,
        /// A description of the join failure.
        message: String,
    },
    /// Offline mode requires an alias that has not been resolved and stored.
    #[error("offline content is missing: action ref {owner}/{repo}@{git_ref}")]
    OfflineMissing {
        /// Repository owner.
        owner: String,
        /// Repository name.
        repo: String,
        /// Missing mutable ref.
        git_ref: String,
    },
}

/// Resolves `git_ref` against `owner/repo`, short-circuiting to
/// [`CommitSha::parse`] with zero I/O when `git_ref` already looks like a
/// full 40-hex commit SHA.
///
/// This is the one function callers use regardless of which concrete
/// [`RefResolver`] backs `resolver` — see the module docs for why the
/// SHA-passthrough lives here rather than in every implementation.
///
/// # Errors
/// Returns [`ResolveError`] when `git_ref` is not already a SHA and
/// `resolver` fails to resolve it.
pub async fn resolve_ref(
    resolver: &dyn RefResolver,
    owner: &str,
    repo: &str,
    git_ref: &str,
) -> Result<CommitSha, ResolveError> {
    if CommitSha::looks_like_sha(git_ref) {
        // `CommitSha::parse` cannot fail here: `looks_like_sha` already
        // checked the exact same grammar it validates.
        if let Ok(sha) = CommitSha::parse(git_ref) {
            return Ok(sha);
        }
    }
    resolver.resolve(owner, repo, git_ref).await
}
