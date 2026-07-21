//! `jobs.<id>.steps[]:` — a single step.

use crate::model::value::ScalarOrExpr;
use crate::span::{Span, Spanned};

/// One step. Covers `id`, `if`, `name`, `uses`, `run`, `shell`, `with`,
/// `env`, `working-directory`, `continue-on-error`, `timeout-minutes` per
/// `PHASE-1-engine-core.md`. Exactly one of `run`/`uses` is present, held in
/// [`StepAction`].
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The whole step's span.
    pub span: Span,
    /// `id:`.
    pub id: Option<Spanned<String>>,
    /// `if:` — raw expression text, with or without the `${{ }}` wrapper.
    pub if_condition: Option<Spanned<String>>,
    /// `name:` — display name.
    pub name: Option<Spanned<ScalarOrExpr>>,
    /// `env:`.
    pub env: Vec<(Spanned<String>, Spanned<ScalarOrExpr>)>,
    /// `working-directory:`.
    pub working_directory: Option<Spanned<ScalarOrExpr>>,
    /// `continue-on-error:`.
    pub continue_on_error: Option<Spanned<ScalarOrExpr>>,
    /// `timeout-minutes:`.
    pub timeout_minutes: Option<Spanned<ScalarOrExpr>>,
    /// Whether this is a `run:` or a `uses:` step, and that key's fields.
    pub action: StepAction,
}

/// A step is either a shell command (`run:`) or an action invocation
/// (`uses:`) — GitHub requires exactly one of the two.
#[derive(Debug, Clone, PartialEq)]
pub enum StepAction {
    /// A `run:` step.
    Run {
        /// The script body, verbatim. Never itself an `${{ }}`-classified
        /// [`ScalarOrExpr`]: script text routinely mixes shell code with
        /// zero or more embedded `${{ … }}` placeholders, so it is always
        /// raw text — see `crate::extract` for how those placeholders are
        /// still found for static extraction.
        script: Spanned<String>,
        /// `shell:`.
        shell: Option<Spanned<String>>,
    },
    /// A `uses:` step.
    Uses {
        /// The action reference (e.g. `actions/checkout@v4`). Never an
        /// expression in valid GitHub syntax for a regular action, so kept
        /// as raw text rather than [`ScalarOrExpr`].
        reference: Spanned<String>,
        /// `with:`.
        with: Vec<(Spanned<String>, Spanned<ScalarOrExpr>)>,
    },
}
