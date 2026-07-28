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
/// Generic over the two strategies rather than hardcoding them; production
/// composes the real tarball and Git-clone boundaries.
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
