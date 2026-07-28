//! The content-addressed action source store, `~/.litci/actions/<owner>/<repo>/<sha>/`.
//!
//! `PHASE-3-actions.md`: "Fetch action source… into a content-addressed
//! action store at `~/.litci/actions/<owner>/<repo>/<sha>/`. Never re-fetch
//! a stored SHA." [`ActionStore`] owns presence-checking (so a second run
//! performs zero network fetches for a SHA it already has) and atomic-ish
//! installation (fetch to a temp sibling, then rename into place, so a
//! killed fetch never leaves a half-populated SHA directory that would be
//! mistaken for a complete one).
//!
//! # Which `uses:` forms use this store
//!
//! Only [`crate::ActionRef::Repository`] does. The other two forms
//! deliberately bypass it:
//! - [`crate::ActionRef::Local`] names a path already present in the
//!   checked-out workspace this run is executing — there is nothing to
//!   fetch; a later task group reads it directly from the isolated
//!   workspace.
//! - [`crate::ActionRef::Docker`] names an image, not source code; pulling
//!   and caching it is the container engine's job (`greenlit-runtime`'s
//!   `ContainerEngine`), which already has its own image store.

mod extract;
mod fallback;
mod fetcher;
mod git_clone;
mod tarball;

pub use fallback::FallbackFetcher;
pub use fetcher::{ActionFetcher, FetchError, OfflineActionFetcher};
pub use git_clone::GitCloneFetcher;
pub use tarball::TarballFetcher;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::Instrument;

use greenlit_store::cas::{CasError, CasStore, ObjectDigest};

use crate::sha::CommitSha;
use crate::stage_span;

/// Whether [`ActionStore::ensure_fetched`] found the SHA already present
/// (no fetcher call at all) or had to fetch it.
///
/// `PHASE-3-actions.md`: "expose hit/miss so callers can count." Callers
/// wiring this into `greenlit-metrics`' `Invocation.record_lookup` (a later
/// task group — this crate deliberately does not depend on
/// `greenlit-metrics` itself) pass `outcome == FetchOutcome::Hit` straight
/// through as that call's `hit: bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The SHA directory already existed; nothing was fetched.
    Hit,
    /// The SHA directory did not exist and was just populated.
    Miss,
}

impl FetchOutcome {
    /// Whether this was a [`FetchOutcome::Hit`].
    #[must_use]
    pub fn is_hit(self) -> bool {
        matches!(self, FetchOutcome::Hit)
    }
}

/// A running tally of [`FetchOutcome`]s, for the end-of-run breakdown
/// `PHASE-3-actions.md` asks for ("action-store hit/miss counters"). Every
/// [`ActionStore`] clone shares the same counters (`Arc`-backed), so a
/// store handed out to concurrent callers still accumulates one total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreCounts {
    /// SHAs already present when asked for.
    pub hits: u64,
    /// SHAs fetched because they were not already present.
    pub misses: u64,
}

#[derive(Debug, Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
}

/// The content-addressed store of fetched action source trees.
#[derive(Debug, Clone)]
pub struct ActionStore {
    root: PathBuf,
    counters: Arc<Counters>,
    cas: Option<CasStore>,
}

impl ActionStore {
    /// The default store root under a given home directory:
    /// `<home>/.litci/actions` (`AGENTS.md`: "User-local state | `~/.litci/`"),
    /// mirroring `greenlit-metrics::MetricsStore::default_path_under`'s
    /// pure, side-effect-free pattern.
    #[must_use]
    pub fn default_path_under(home_dir: &Path) -> PathBuf {
        home_dir.join(".litci").join("actions")
    }

    /// Opens a store backed by an explicit root directory — use this in
    /// tests, and for any future `--actions-dir`-style override.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            counters: Arc::new(Counters::default()),
            cas: None,
        }
    }

    /// Opens an explicit action directory backed by verified canonical trees
    /// in `cas`.
    #[must_use]
    pub fn with_cas(root: impl Into<PathBuf>, cas: CasStore) -> Self {
        Self {
            root: root.into(),
            counters: Arc::new(Counters::default()),
            cas: Some(cas),
        }
    }

    /// Opens the real per-user store, resolving the home directory from the
    /// `HOME` environment variable (v0 targets Linux x86_64 only, where
    /// `HOME` is the standard, always-present convention — see
    /// `greenlit-metrics::MetricsStore::open_default`'s identical
    /// reasoning).
    ///
    /// # Errors
    /// Returns [`StoreError::HomeDirUnavailable`]/[`StoreError::InvalidHomeDir`]
    /// when `HOME` is unset or not an absolute path.
    pub fn open_default() -> Result<Self, StoreError> {
        let home = std::env::var_os("HOME").ok_or(StoreError::HomeDirUnavailable)?;
        let home = Path::new(&home);
        if !home.is_absolute() {
            return Err(StoreError::InvalidHomeDir);
        }
        let cas =
            CasStore::open(CasStore::default_path_under(home)).map_err(StoreError::Content)?;
        Ok(Self::with_cas(Self::default_path_under(home), cas))
    }

    /// The store's root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The current hit/miss tally across every [`ActionStore::ensure_fetched`]
    /// call made through this store (and any clone of it).
    #[must_use]
    pub fn counts(&self) -> StoreCounts {
        StoreCounts {
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
        }
    }

    /// Where `owner/repo@sha` lives (or would live) in the store.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidComponent`] if `owner`/`repo` are empty
    /// or contain a path separator or `..` — defense in depth against a
    /// caller that did not go through [`crate::ActionRef::parse`] (which
    /// already guarantees this).
    pub fn action_dir(
        &self,
        owner: &str,
        repo: &str,
        sha: &CommitSha,
    ) -> Result<PathBuf, StoreError> {
        validate_component(owner)?;
        validate_component(repo)?;
        Ok(self.root.join(owner).join(repo).join(sha.as_str()))
    }

    /// Whether `owner/repo@sha` is already present in the store.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidComponent`]; see [`ActionStore::action_dir`].
    pub fn has(&self, owner: &str, repo: &str, sha: &CommitSha) -> Result<bool, StoreError> {
        Ok(self.action_dir(owner, repo, sha)?.is_dir())
    }

    /// Ensures `owner/repo@sha`'s source tree is present in the store,
    /// fetching it with `fetcher` only on a miss, and returns its directory
    /// alongside which happened.
    ///
    /// A killed/panicked fetch can never leave a half-populated SHA
    /// directory that a later call would treat as a hit: the fetch lands in
    /// a temporary sibling directory first (named so it can never collide
    /// with a real 40-hex SHA directory name) and is renamed into place only
    /// once `fetcher` returns success.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidComponent`] (see [`ActionStore::action_dir`]),
    /// an I/O [`StoreError`] variant if creating/installing the directory
    /// fails, or [`StoreError::Fetch`] if `fetcher` itself fails.
    pub async fn ensure_fetched(
        &self,
        owner: &str,
        repo: &str,
        sha: &CommitSha,
        fetcher: &dyn ActionFetcher,
    ) -> Result<(PathBuf, FetchOutcome), StoreError> {
        let span = stage_span("action-fetch");
        async move {
            let dest = self.action_dir(owner, repo, sha)?;
            let alias = format!("{owner}/{repo}@{}", sha.as_str());
            if dest.is_dir() {
                self.verify_or_migrate(&alias, &dest).await?;
                self.counters.hits.fetch_add(1, Ordering::Relaxed);
                return Ok((dest, FetchOutcome::Hit));
            }

            if self.restore_from_cas(&alias, &dest).await? {
                self.counters.hits.fetch_add(1, Ordering::Relaxed);
                return Ok((dest, FetchOutcome::Hit));
            }

            let parent = dest.parent().ok_or_else(|| StoreError::InvalidComponent {
                component: dest.display().to_string(),
                reason: "action directory has no parent",
            })?;
            std::fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;

            let tmp = parent.join(temp_sibling_name());
            std::fs::create_dir_all(&tmp).map_err(|source| StoreError::CreateDir {
                path: tmp.clone(),
                source,
            })?;

            if let Err(error) = fetcher.fetch(owner, repo, sha, &tmp).await {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(StoreError::Fetch(error));
            }
            if let Err(error) = self.verify_or_migrate(&alias, &tmp).await {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(error);
            }

            match std::fs::rename(&tmp, &dest) {
                Ok(()) => {
                    self.counters.misses.fetch_add(1, Ordering::Relaxed);
                    Ok((dest, FetchOutcome::Miss))
                }
                Err(_) if dest.is_dir() => {
                    // Another concurrent `litci` process finished fetching
                    // the same SHA first; that is just as good a hit as one
                    // that was already there when this call started.
                    let _ = std::fs::remove_dir_all(&tmp);
                    self.counters.hits.fetch_add(1, Ordering::Relaxed);
                    Ok((dest, FetchOutcome::Hit))
                }
                Err(source) => {
                    let _ = std::fs::remove_dir_all(&tmp);
                    Err(StoreError::Install { path: dest, source })
                }
            }
        }
        .instrument(span)
        .await
    }

    async fn verify_or_migrate(&self, alias: &str, path: &Path) -> Result<(), StoreError> {
        let Some(cas) = self.cas.clone() else {
            return Ok(());
        };
        let alias = alias.to_string();
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let previous = cas.resolve_alias("action-commit", &alias)?;
            let actual = if previous.is_some() {
                cas.tree_digest(&path)?
            } else {
                cas.put_tree(&path)?
            };
            if let Some(expected) = previous
                && expected != actual
            {
                return Err(StoreError::ContentIdentityChanged {
                    alias,
                    expected,
                    actual,
                });
            }
            cas.record_alias("action-commit", &alias, &actual)?;
            Ok(())
        })
        .await
        .map_err(|error| StoreError::Task(error.to_string()))?
    }

    async fn restore_from_cas(&self, alias: &str, dest: &Path) -> Result<bool, StoreError> {
        let Some(cas) = self.cas.clone() else {
            return Ok(false);
        };
        let Some(digest) = cas
            .resolve_alias("action-commit", alias)
            .map_err(StoreError::Content)?
        else {
            return Ok(false);
        };
        let parent = dest.parent().ok_or_else(|| StoreError::InvalidComponent {
            component: dest.display().to_string(),
            reason: "action directory has no parent",
        })?;
        std::fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
        let tmp = parent.join(temp_sibling_name());
        std::fs::create_dir_all(&tmp).map_err(|source| StoreError::CreateDir {
            path: tmp.clone(),
            source,
        })?;
        let cas_for_task = cas.clone();
        let digest_for_task = digest.clone();
        let tmp_for_task = tmp.clone();
        let materialized = tokio::task::spawn_blocking(move || {
            cas_for_task.materialize_tree(&digest_for_task, &tmp_for_task)
        })
        .await
        .map_err(|error| StoreError::Task(error.to_string()))?;
        if let Err(error) = materialized {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(StoreError::Content(error));
        }
        match std::fs::rename(&tmp, dest) {
            Ok(()) => Ok(true),
            Err(_) if dest.is_dir() => {
                let _ = std::fs::remove_dir_all(&tmp);
                self.verify_or_migrate(alias, dest).await?;
                Ok(true)
            }
            Err(source) => {
                let _ = std::fs::remove_dir_all(&tmp);
                Err(StoreError::Install {
                    path: dest.to_path_buf(),
                    source,
                })
            }
        }
    }
}

/// A directory name that can never collide with a 40-hex-character SHA
/// directory name (its `.tmp-` prefix alone rules that out), unique enough
/// per-process-per-call to avoid colliding with a concurrent fetch of a
/// *different* SHA under the same `owner/repo`. Two concurrent fetches of
/// the *same* SHA racing each other is handled by
/// [`ActionStore::ensure_fetched`]'s rename-failure branch, not by this name
/// being globally unique.
fn temp_sibling_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".tmp-{}-{}-{nanos}", std::process::id(), counter)
}

fn validate_component(component: &str) -> Result<(), StoreError> {
    if component.is_empty() {
        return Err(StoreError::InvalidComponent {
            component: component.to_owned(),
            reason: "must not be empty",
        });
    }
    if component == "." || component == ".." {
        return Err(StoreError::InvalidComponent {
            component: component.to_owned(),
            reason: "must not be '.' or '..'",
        });
    }
    if component.contains('/') || component.contains('\0') {
        return Err(StoreError::InvalidComponent {
            component: component.to_owned(),
            reason: "must not contain a path separator or NUL byte",
        });
    }
    Ok(())
}

/// A failure managing the content-addressed action store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// [`ActionStore::open_default`] could not resolve a home directory
    /// because `HOME` is unset.
    #[error(
        "could not determine the user home directory (the HOME environment variable is not set)"
    )]
    HomeDirUnavailable,
    /// `HOME` was present but not an absolute path.
    #[error("could not determine the user home directory (HOME is not an absolute path)")]
    InvalidHomeDir,
    /// An `owner`/`repo` string was empty or path-unsafe.
    #[error("invalid action-store path component '{component}': {reason}")]
    InvalidComponent {
        /// The offending value.
        component: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// Creating a store directory failed.
    #[error("failed to create action store directory {path}: {source}")]
    CreateDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Renaming a completed fetch into its final content-addressed path
    /// failed for a reason other than losing a race with a concurrent
    /// fetch of the same SHA (that case is not an error — see
    /// [`ActionStore::ensure_fetched`]).
    #[error("failed to install fetched action into {path}: {source}")]
    Install {
        /// The destination path that could not be installed.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The injected [`ActionFetcher`] itself failed.
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// The machine-wide verified content store failed.
    #[error("verified action content: {0}")]
    Content(#[from] CasError),
    /// A supposedly immutable action commit resolved to different source
    /// bytes than the tree already recorded for it.
    #[error(
        "action {alias} changed content after it was locked (expected {expected}, found {actual})"
    )]
    ContentIdentityChanged {
        /// Immutable action commit alias.
        alias: String,
        /// Previously recorded tree identity.
        expected: ObjectDigest,
        /// Newly observed tree identity.
        actual: ObjectDigest,
    },
    /// A blocking content task did not complete.
    #[error("verified action content task did not complete: {0}")]
    Task(String),
}
