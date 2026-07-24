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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn bare_remote_with_one_commit() -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let bare = root.path().join("owner").join("repo.git");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        run(work.path(), &["init", "-q", "-b", "main"]);
        run(work.path(), &["config", "user.email", "t@example.com"]);
        run(work.path(), &["config", "user.name", "t"]);
        std::fs::write(work.path().join("action.yml"), "name: test\n").unwrap();
        run(work.path(), &["add", "."]);
        run(work.path(), &["commit", "-q", "-m", "one"]);
        run(
            work.path(),
            &[
                "clone",
                "-q",
                "--bare",
                work.path().to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let sha = {
            let output = std::process::Command::new("git")
                .args(["-C", bare.to_str().unwrap(), "rev-parse", "refs/heads/main"])
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        (root, sha)
    }

    #[tokio::test]
    async fn clones_the_pinned_commit_and_strips_git_metadata() {
        let (root, sha) = bare_remote_with_one_commit();
        let fetcher = GitCloneFetcher::with_base_url(root.path().to_str().unwrap());
        let dest = tempfile::tempdir().unwrap();
        let commit = CommitSha::parse(&sha).unwrap();
        fetcher
            .fetch("owner", "repo", &commit, dest.path())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.path().join("action.yml")).unwrap(),
            "name: test\n"
        );
        assert!(!dest.path().join(".git").exists());
    }

    #[tokio::test]
    async fn missing_repository_is_a_clone_error() {
        let root = tempfile::tempdir().unwrap();
        let fetcher = GitCloneFetcher::with_base_url(root.path().to_str().unwrap());
        let dest = tempfile::tempdir().unwrap();
        let commit = CommitSha::parse(&"a".repeat(40)).unwrap();
        let error = fetcher
            .fetch("owner", "does-not-exist", &commit, dest.path())
            .await
            .unwrap_err();
        assert!(matches!(error, FetchError::Clone { .. }));
    }
}
