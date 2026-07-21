//! The workflow root and the `env`/`defaults`/`permissions` shapes shared
//! between the workflow and job levels.

use crate::model::job::Job;
use crate::model::trigger::Trigger;
use crate::model::value::{ScalarOrExpr, UnsupportedConstruct};
use crate::span::{Span, Spanned};

/// A fully parsed `.github/workflows/*.yml` file.
///
/// Covers everything `PHASE-1-engine-core.md`'s greenlit-workflow section
/// lists: `on` (all trigger forms), `env`, `defaults`, `permissions`, and
/// `jobs`. `concurrency` is recognized but not deeply modeled (see
/// [`UnsupportedConstruct`]); `run-name` and job-level `permissions` are not
/// modeled at all in v0 (see the crate-level docs' "Known limitations").
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    /// The whole-document span.
    pub span: Span,
    /// `name:` — display name. Plain text; GitHub does not evaluate
    /// expressions in the workflow-level `name` key (unlike `run-name`,
    /// which this crate does not model — see crate docs).
    pub name: Option<Spanned<String>>,
    /// `on:` — every trigger, normalized from whichever of the three YAML
    /// forms was used.
    pub on: Vec<Spanned<Trigger>>,
    /// `env:` at the workflow level.
    pub env: Vec<(Spanned<String>, Spanned<ScalarOrExpr>)>,
    /// `defaults:`.
    pub defaults: Option<Spanned<Defaults>>,
    /// `permissions:`.
    pub permissions: Option<Spanned<Permissions>>,
    /// `concurrency:`, recognized but not deeply parsed — see
    /// [`UnsupportedConstruct`].
    pub concurrency: Option<UnsupportedConstruct>,
    /// `jobs:`, in file order.
    pub jobs: Vec<Job>,
}

/// `defaults:` (workflow- or job-level — identical shape at both levels).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Defaults {
    /// `defaults.run:`.
    pub run: Option<Spanned<RunDefaults>>,
}

/// `defaults.run:`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunDefaults {
    /// `defaults.run.shell:`.
    pub shell: Option<Spanned<String>>,
    /// `defaults.run.working-directory:`.
    pub working_directory: Option<Spanned<ScalarOrExpr>>,
}

/// `permissions:` at the workflow level (job-level `permissions:` is not
/// modeled in v0 — see crate docs' "Known limitations").
#[derive(Debug, Clone, PartialEq)]
pub enum Permissions {
    /// `permissions: read-all` / `permissions: write-all`.
    All(PermissionLevelAll),
    /// `permissions: {}` is `Scoped(vec![])`; `permissions:` with no value
    /// is also schema-valid and modeled the same way as an empty scope set
    /// (GitHub docs: omitting a scope defaults it to `none`).
    Scoped(Vec<(Spanned<String>, Spanned<PermissionLevel>)>),
}

/// `permissions: read-all` / `write-all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionLevelAll {
    /// `permissions: read-all`.
    ReadAll,
    /// `permissions: write-all`.
    WriteAll,
}

/// A single scope's level under `permissions: { <scope>: <level> }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionLevel {
    /// `read`.
    Read,
    /// `write`.
    Write,
    /// `none`.
    None,
}
