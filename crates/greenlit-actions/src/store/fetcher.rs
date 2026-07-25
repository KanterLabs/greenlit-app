//! The injected fetch boundary and its shared error type.

use std::path::Path;

use async_trait::async_trait;

use crate::sha::CommitSha;

/// Populates an already-created, empty destination directory with an
/// action's unpacked source tree for one `owner/repo@sha`.
///
/// Implementations perform real I/O (a tarball download, a `git clone`);
/// [`crate::store::ActionStore::ensure_fetched`] drives this trait rather
/// than owning the fetch strategy itself, so tests can inject a fake that
/// writes a few files directly, exercising the store's presence-check/
/// atomic-install/hit-miss-counting logic without any network or `git`
/// process at all.
#[async_trait]
pub trait ActionFetcher: Send + Sync {
    /// Fetches `owner/repo` at `sha` into `dest`, which
    /// [`crate::store::ActionStore`] guarantees exists, is empty, and is a
    /// sibling of (not yet renamed into) the final content-addressed path.
    ///
    /// Implementations must not assume `dest`'s final location — only that
    /// it is currently a writable, empty directory.
    ///
    /// # Errors
    /// Returns [`FetchError`] on any download, extraction, or clone
    /// failure.
    async fn fetch(
        &self,
        owner: &str,
        repo: &str,
        sha: &CommitSha,
        dest: &Path,
    ) -> Result<(), FetchError>;
}

/// Fetch boundary used for offline replay.
///
/// A cached action never calls this boundary. Reaching it therefore proves
/// that the exact resolved action source is absent locally.
#[derive(Debug, Clone, Copy, Default)]
pub struct OfflineActionFetcher;

#[async_trait]
impl ActionFetcher for OfflineActionFetcher {
    async fn fetch(
        &self,
        owner: &str,
        repo: &str,
        sha: &CommitSha,
        _dest: &Path,
    ) -> Result<(), FetchError> {
        Err(FetchError::OfflineMissing {
            owner: owner.to_string(),
            repo: repo.to_string(),
            sha: sha.as_str().to_string(),
        })
    }
}

/// A failure fetching an action's source into the store.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Offline mode requires action source that is absent locally.
    #[error("offline content is missing: action source {owner}/{repo}@{sha}")]
    OfflineMissing {
        /// Repository owner.
        owner: String,
        /// Repository name.
        repo: String,
        /// Exact missing commit.
        sha: String,
    },
    /// The tarball could not be downloaded (network/HTTP failure).
    #[error("could not download {owner}/{repo}@{sha}: {message}")]
    Download {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The commit being fetched.
        sha: String,
        /// A bounded, safe-to-display diagnostic.
        message: String,
    },
    /// The downloaded tarball could not be safely extracted.
    #[error("could not extract {owner}/{repo}@{sha}: {message}")]
    Extract {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The commit being fetched.
        sha: String,
        /// A bounded, safe-to-display diagnostic.
        message: String,
    },
    /// `git` could not clone/fetch/checkout the commit.
    #[error("could not clone {owner}/{repo}@{sha}: {message}")]
    Clone {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The commit being fetched.
        sha: String,
        /// A bounded, safe-to-display diagnostic.
        message: String,
    },
    /// A network/process operation did not complete within its deadline.
    #[error("fetching {owner}/{repo}@{sha} exceeded the {seconds}-second deadline")]
    TimedOut {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The commit being fetched.
        sha: String,
        /// The deadline in seconds.
        seconds: u64,
    },
    /// Both the tarball and the `git clone` fallback failed —
    /// [`crate::store::FallbackFetcher`]'s only variant.
    #[error(
        "could not fetch {owner}/{repo}@{sha}: tarball download failed ({tarball_error}); git clone fallback also failed ({clone_error})"
    )]
    AllStrategiesFailed {
        /// The repository owner.
        owner: String,
        /// The repository name.
        repo: String,
        /// The commit being fetched.
        sha: String,
        /// The tarball strategy's failure.
        tarball_error: String,
        /// The git-clone strategy's failure.
        clone_error: String,
    },
}
