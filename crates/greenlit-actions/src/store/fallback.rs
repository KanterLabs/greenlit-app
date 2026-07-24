//! Composes the tarball and `git clone` strategies, trying the former first.

use std::path::Path;

use async_trait::async_trait;

use crate::sha::CommitSha;
use crate::store::fetcher::{ActionFetcher, FetchError};

/// Tries a tarball-download strategy, falling back to a `git clone`
/// strategy when it fails — `PHASE-3-actions.md`: "tarball download…
/// falling back to shallow git clone." This is the fetcher real callers
/// should construct, typically as
/// `FallbackFetcher::new(TarballFetcher::new(), GitCloneFetcher::new())`;
/// [`crate::store::TarballFetcher`] and [`crate::store::GitCloneFetcher`]
/// remain independently public so a caller can force one strategy only.
///
/// Generic over the two strategies (rather than hardcoding the two
/// concrete types) so this composition rule — clearing any partial output
/// the first strategy left behind before handing `dest` to the second — is
/// itself testable against [`ActionFetcher`] fakes, the same way every
/// other true-external boundary in this crate is.
pub struct FallbackFetcher {
    tarball: Box<dyn ActionFetcher>,
    git_clone: Box<dyn ActionFetcher>,
}

impl FallbackFetcher {
    /// Builds the composite fetcher from a tarball strategy and a
    /// git-clone fallback strategy.
    pub fn new(
        tarball: impl ActionFetcher + 'static,
        git_clone: impl ActionFetcher + 'static,
    ) -> Self {
        Self {
            tarball: Box::new(tarball),
            git_clone: Box::new(git_clone),
        }
    }
}

#[async_trait]
impl ActionFetcher for FallbackFetcher {
    async fn fetch(
        &self,
        owner: &str,
        repo: &str,
        sha: &CommitSha,
        dest: &Path,
    ) -> Result<(), FetchError> {
        let tarball_error = match self.tarball.fetch(owner, repo, sha, dest).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };

        // The tarball attempt may have partially populated `dest` before
        // failing; clear it so the clone strategy starts from a genuinely
        // empty directory, preserving the same "all-or-nothing" contract
        // `ActionFetcher` documents.
        if let Err(error) = clear_directory(dest) {
            return Err(FetchError::AllStrategiesFailed {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                sha: sha.as_str().to_owned(),
                tarball_error: tarball_error.to_string(),
                clone_error: format!("could not reset destination for fallback: {error}"),
            });
        }

        match self.git_clone.fetch(owner, repo, sha, dest).await {
            Ok(()) => Ok(()),
            Err(clone_error) => Err(FetchError::AllStrategiesFailed {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                sha: sha.as_str().to_owned(),
                tarball_error: tarball_error.to_string(),
                clone_error: clone_error.to_string(),
            }),
        }
    }
}

fn clear_directory(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake first strategy that partially writes a file before failing,
    /// so the assertion below can prove the fallback clears it.
    struct PartialThenFail;

    #[async_trait]
    impl ActionFetcher for PartialThenFail {
        async fn fetch(
            &self,
            _owner: &str,
            _repo: &str,
            _sha: &CommitSha,
            dest: &Path,
        ) -> Result<(), FetchError> {
            std::fs::write(dest.join("partial.txt"), b"leftover").unwrap();
            Err(FetchError::Download {
                owner: "owner".into(),
                repo: "repo".into(),
                sha: "sha".into(),
                message: "simulated failure".into(),
            })
        }
    }

    /// A fake second strategy that asserts `dest` was cleared before it
    /// runs, then succeeds.
    struct AssertsCleanThenSucceed;

    #[async_trait]
    impl ActionFetcher for AssertsCleanThenSucceed {
        async fn fetch(
            &self,
            _owner: &str,
            _repo: &str,
            _sha: &CommitSha,
            dest: &Path,
        ) -> Result<(), FetchError> {
            assert!(
                std::fs::read_dir(dest).unwrap().next().is_none(),
                "destination must be empty before the fallback strategy runs"
            );
            std::fs::write(dest.join("action.yml"), b"name: ok").unwrap();
            Ok(())
        }
    }

    struct AlwaysFail;

    #[async_trait]
    impl ActionFetcher for AlwaysFail {
        async fn fetch(
            &self,
            owner: &str,
            repo: &str,
            sha: &CommitSha,
            _dest: &Path,
        ) -> Result<(), FetchError> {
            Err(FetchError::Clone {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                sha: sha.as_str().to_owned(),
                message: "simulated clone failure".into(),
            })
        }
    }

    #[tokio::test]
    async fn falls_back_after_clearing_partial_output() {
        let fetcher = FallbackFetcher::new(PartialThenFail, AssertsCleanThenSucceed);
        let dest = tempfile::tempdir().unwrap();
        let sha = CommitSha::parse(&"a".repeat(40)).unwrap();
        fetcher
            .fetch("owner", "repo", &sha, dest.path())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.path().join("action.yml")).unwrap(),
            "name: ok"
        );
        assert!(!dest.path().join("partial.txt").exists());
    }

    #[tokio::test]
    async fn reports_both_failures_when_every_strategy_fails() {
        let fetcher = FallbackFetcher::new(PartialThenFail, AlwaysFail);
        let dest = tempfile::tempdir().unwrap();
        let sha = CommitSha::parse(&"a".repeat(40)).unwrap();
        let error = fetcher
            .fetch("owner", "repo", &sha, dest.path())
            .await
            .unwrap_err();
        match error {
            FetchError::AllStrategiesFailed {
                tarball_error,
                clone_error,
                ..
            } => {
                assert!(tarball_error.contains("simulated failure"));
                assert!(clone_error.contains("simulated clone failure"));
            }
            other => panic!("expected AllStrategiesFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn first_strategy_success_never_invokes_the_second() {
        struct Unreachable;
        #[async_trait]
        impl ActionFetcher for Unreachable {
            async fn fetch(
                &self,
                _owner: &str,
                _repo: &str,
                _sha: &CommitSha,
                _dest: &Path,
            ) -> Result<(), FetchError> {
                panic!("the fallback strategy must not run when the first succeeds");
            }
        }
        struct Succeeds;
        #[async_trait]
        impl ActionFetcher for Succeeds {
            async fn fetch(
                &self,
                _owner: &str,
                _repo: &str,
                _sha: &CommitSha,
                dest: &Path,
            ) -> Result<(), FetchError> {
                std::fs::write(dest.join("action.yml"), b"name: ok").unwrap();
                Ok(())
            }
        }
        let fetcher = FallbackFetcher::new(Succeeds, Unreachable);
        let dest = tempfile::tempdir().unwrap();
        let sha = CommitSha::parse(&"a".repeat(40)).unwrap();
        fetcher
            .fetch("owner", "repo", &sha, dest.path())
            .await
            .unwrap();
    }
}
