#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! `greenlit-actions`: marketplace-action support for `litci run`.
//!
//! This crate implements `PHASE-3-actions.md`'s "Action resolution" task
//! group — everything needed to turn a workflow step's `uses:` value into a
//! ready-to-execute action source tree on disk, ahead of running it:
//!
//! 1. [`ActionRef::parse`] — parse `uses:` into a typed reference:
//!    `owner/repo@ref`, `owner/repo/subdir@ref`, `./local/path`, or
//!    `docker://image:tag`.
//! 2. [`resolve::resolve_ref`] — resolve a [`ActionRef::Repository`]'s ref
//!    (tag, branch, or SHA) to a full commit [`CommitSha`], via the GitHub
//!    API ([`resolve::GitHubApiResolver`]) or tokenless `git ls-remote`
//!    ([`resolve::GitLsRemoteResolver`]).
//! 3. [`store::ActionStore`] — fetch the resolved commit's source into the
//!    content-addressed store at `~/.litci/actions/<owner>/<repo>/<sha>/`
//!    ([`store::TarballFetcher`] falling back to [`store::GitCloneFetcher`]
//!    via [`store::FallbackFetcher`]), never re-fetching a SHA already
//!    present.
//! 4. [`manifest::load_manifest`] — parse the fetched (or local/`./path`)
//!    action's `action.yml`/`action.yaml` into a typed
//!    [`manifest::ActionManifest`].
//!
//! Actually *executing* a resolved action (JavaScript/composite/Docker) is
//! the next Phase 3 task group's job, not this crate's, per
//! `PHASE-3-actions.md`'s own task grouping — this crate's output (a
//! directory containing the action's source plus its parsed manifest) is
//! that task group's input.
//!
//! # Metrics
//!
//! `resolve`/`store` operations open `tracing` spans matching
//! `greenlit-runtime`'s executor convention
//! (`target: "greenlit_metrics::timed_stage"`, stages `"action-resolve"`/
//! `"action-fetch"`), so `greenlit-metrics`' `Invocation` timing layer
//! records them without this crate depending on `greenlit-metrics` itself
//! (`PHASE-3-actions.md`: "Instrument spans for resolve and fetch"). Hit/
//! miss counting is exposed directly from [`store::ActionStore::counts`]
//! rather than by this crate calling into `greenlit-metrics`, per the
//! phase brief: "do not modify greenlit-metrics' record schema in this task
//! group... expose the counts from the store API instead."

mod gitproc;
pub mod manifest;
pub mod resolve;
mod sha;
pub mod store;
mod uses_ref;

pub use sha::{COMMIT_SHA_LENGTH, CommitSha, InvalidCommitSha};
pub use uses_ref::{ActionRef, RepositoryRef, UsesRefError};

/// Opens a `tracing` span named to match `greenlit-runtime`'s executor
/// convention (`crates/greenlit-runtime/src/executor/mod.rs::stage_span`),
/// so `greenlit-metrics`' `Invocation` timing layer records this crate's
/// stages without a dependency edge back onto `greenlit-metrics`. Shared by
/// [`resolve`] (`"action-resolve"`) and [`store`] (`"action-fetch"`).
pub(crate) fn stage_span(stage: &'static str) -> tracing::Span {
    tracing::info_span!(
        target: "greenlit_metrics::timed_stage",
        "greenlit_stage",
        stage = stage
    )
}
