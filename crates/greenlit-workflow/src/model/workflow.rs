//! The workflow root and the `env`/`defaults`/`permissions` shapes shared
//! between the workflow and job levels.

use crate::model::job::Job;
use crate::model::trigger::Trigger;
use crate::model::value::ScalarOrExpr;
use crate::span::{Span, Spanned};

/// A fully parsed `.github/workflows/*.yml` file.
///
/// Covers everything `PHASE-1-engine-core.md`'s greenlit-workflow section
/// lists: `run-name`, `on` (all trigger forms), `env`, `defaults`,
/// `permissions`, `concurrency`, and `jobs`.
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    /// The whole-document span.
    pub span: Span,
    /// `name:` — display name. Plain text; GitHub does not evaluate
    /// expressions in the workflow-level `name` key.
    pub name: Option<Spanned<String>>,
    /// `run-name:` — workflow-run display name. GitHub evaluates template
    /// expressions here using only the `github`, `inputs`, and `vars`
    /// contexts. An omitted value, or one that resolves to only whitespace,
    /// uses GitHub's event-specific default run name.
    pub run_name: Option<Spanned<String>>,
    /// `on:` — every trigger, normalized from whichever of the three YAML
    /// forms was used.
    pub on: Vec<Spanned<Trigger>>,
    /// `env:` at the workflow level.
    pub env: Vec<(Spanned<String>, Spanned<ScalarOrExpr>)>,
    /// `defaults:`.
    pub defaults: Option<Spanned<Defaults>>,
    /// `permissions:`.
    pub permissions: Option<Spanned<Permissions>>,
    /// Workflow concurrency group and cancellation policy.
    pub concurrency: Option<Spanned<Concurrency>>,
    /// `jobs:`, in file order.
    pub jobs: Vec<Job>,
}

/// A workflow- or job-level `concurrency:` declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct Concurrency {
    /// Group key, including any template expressions.
    pub group: Spanned<String>,
    /// Whether a newer owner cancels the current owner. Omitted means false.
    pub cancel_in_progress: Option<Spanned<ScalarOrExpr>>,
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

/// A workflow- or job-level `permissions:` declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum Permissions {
    /// `permissions: read-all` / `permissions: write-all`.
    All(PermissionLevelAll),
    /// `permissions: {}` is `Scoped(vec![])`; omitted scopes are `none`.
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
