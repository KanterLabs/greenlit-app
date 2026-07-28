//! Fetching an action's source with a shallow `git` clone — the fallback
//! path when a tarball download fails.
//!
//! `PHASE-3-actions.md`: "Fetch action source (tarball download; fall back
//! to shallow git clone)." GitHub's remotes accept fetching an exact commit
//! SHA directly (not just named refs), so this needs no branch/tag name at
//! all: `git init` an empty repository, `git fetch --depth 1 <url> <sha>`,
//! then `git checkout FETCH_HEAD` to populate the working tree. The
//! resulting `.git` metadata directory is removed afterward so the stored
//! tree matches the tarball path's shape (just the action's source files).

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tracing::Instrument;

use crate::gitproc::{self, GitProcessError};
use crate::sha::CommitSha;
use crate::stage_span;
use crate::store::fetcher::{ActionFetcher, FetchError};

/// A shallow clone of one commit is still a real network operation;
/// generous relative to `git_ls_remote`'s deadline since a clone transfers
/// more data than a ref listing.
const CLONE_TIMEOUT: Duration = Duration::from_secs(120);

/// Fetches by shallow-cloning `https://github.com/<owner>/<repo>.git` (or,
/// for tests, a caller-supplied base — see
/// [`crate::resolve::GitLsRemoteResolver::with_base_url`]).
#[derive(Debug, Clone)]
pub struct GitCloneFetcher {
    base_url: String,
}

impl GitCloneFetcher {
    /// A fetcher that clones from real GitHub over HTTPS.
    #[must_use]
    pub fn new() -> Self {
        Self {
            base_url: "https://github.com".to_owned(),
        }
    }

    /// A fetcher against `<base>/<owner>/<repo>.git` instead of GitHub, for
    /// tests.
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Default for GitCloneFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ActionFetcher for GitCloneFetcher {
    async fn fetch(
        &self,
        owner: &str,
        repo: &str,
        sha: &CommitSha,
        dest: &Path,
    ) -> Result<(), FetchError> {
        let url = format!("{}/{owner}/{repo}.git", self.base_url);
        let dest = dest.to_path_buf();
        let owner_o = owner.to_owned();
        let repo_o = repo.to_owned();
        let sha_o = sha.as_str().to_owned();
        let span = stage_span("action-fetch");
        async move {
            tokio::task::spawn_blocking(move || {
                clone_blocking(&url, &dest, &owner_o, &repo_o, &sha_o)
            })
            .await
            .map_err(|join_error| FetchError::Clone {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                sha: sha.as_str().to_owned(),
                message: format!("clone task did not complete: {join_error}"),
            })?
        }
        .instrument(span)
        .await
    }
}

fn clone_blocking(
    url: &str,
    dest: &Path,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Result<(), FetchError> {
    run_step(&["init", "-q"], dest, owner, repo, sha)?;
    run_step(
        &["fetch", "-q", "--depth", "1", url, sha],
        dest,
        owner,
        repo,
        sha,
    )?;
    run_step(&["checkout", "-q", "FETCH_HEAD"], dest, owner, repo, sha)?;

    let git_dir = dest.join(".git");
    if git_dir.exists() {
        std::fs::remove_dir_all(&git_dir).map_err(|error| FetchError::Clone {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            sha: sha.to_owned(),
            message: format!("could not remove {}: {error}", git_dir.display()),
        })?;
    }
    Ok(())
}

fn run_step(
    args: &[&str],
    dest: &Path,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Result<(), FetchError> {
    let output = gitproc::run_git(Some(dest), args, CLONE_TIMEOUT)
        .map_err(|error| map_process_error(error, owner, repo, sha))?;
    if !output.status.success() {
        return Err(FetchError::Clone {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            sha: sha.to_owned(),
            message: gitproc::diagnostic(&output.stderr, output.stderr_truncated),
        });
    }
    Ok(())
}

fn map_process_error(error: GitProcessError, owner: &str, repo: &str, sha: &str) -> FetchError {
    match error {
        GitProcessError::TimedOut { seconds, .. } => FetchError::TimedOut {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            sha: sha.to_owned(),
            seconds,
        },
        GitProcessError::Spawn { message, .. } | GitProcessError::Io { message, .. } => {
            FetchError::Clone {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                sha: sha.to_owned(),
                message,
            }
        }
    }
}
