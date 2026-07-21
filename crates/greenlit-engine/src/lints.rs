//! [`Lint`]: a plan-time warning that never blocks planning — as opposed to
//! a [`crate::plan::PlanError`], which does.
//!
//! Warnings preserve a valid execution plan while calling attention to
//! suspicious declarations. GitHub documents that dereferencing a missing
//! context property evaluates to an empty string:
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#about-contexts>.
//! It also documents that matrix-job execution order is not guaranteed and
//! that the last matrix job to finish overwrites a duplicate output name:
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#using-job-outputs-in-a-matrix-job>.
//! The non-blocking dead-`exclude` and duplicate-`needs` diagnostics are
//! pinned by
//! `crates/greenlit-app/tests/plan_contracts.rs::dispatch_plan_pins_typed_inputs_layers_skips_zero_legs_and_json_diagnostics`.

use greenlit_workflow::Span;

/// One plan-time warning.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Lint {
    /// Where the lint-worthy construct appears.
    #[serde(serialize_with = "crate::json_shape::serialize_span")]
    pub span: Span,
    /// Human-readable explanation.
    pub message: String,
    /// The lint's kind, for callers that want to filter/group by category
    /// rather than parse `message`.
    pub kind: LintKind,
}

/// A closed set for v0 — every case this crate currently detects. Adding a
/// new lint kind is additive and non-breaking for JSON consumers per the
/// same "unknown kinds are ignored" spirit as [`crate::defer::DeferReason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LintKind {
    /// An `exclude:` entry matched no surviving combination.
    DeadExclude,
    /// `needs.<job>.outputs.<name>` referenced an output `<job>` never
    /// declares.
    UndeclaredNeededOutput,
    /// A matrix job's declared outputs are read by a dependent — the last
    /// leg to write wins, a well-known GHA limitation.
    MatrixOutputsCollision,
    /// The same job id appeared more than once in one `needs:` list. The
    /// planner deduplicates the edge and emits this non-blocking diagnostic.
    DuplicateNeeds,
}

impl Lint {
    pub(crate) fn dead_exclude(span: Span) -> Lint {
        Lint {
            span,
            message: "this `exclude` entry matched no surviving matrix combination".to_string(),
            kind: LintKind::DeadExclude,
        }
    }

    pub(crate) fn undeclared_needed_output(span: Span, job: &str, output: &str) -> Lint {
        Lint {
            span,
            message: format!(
                "job '{job}' declares no output '{output}'; this reference resolves to an empty string at runtime"
            ),
            kind: LintKind::UndeclaredNeededOutput,
        }
    }

    pub(crate) fn matrix_outputs_collision(span: Span, job: &str) -> Lint {
        Lint {
            span,
            message: format!(
                "job '{job}' is a matrix job with declared outputs read by a dependent; the last leg to finish wins (a well-known GitHub Actions limitation)"
            ),
            kind: LintKind::MatrixOutputsCollision,
        }
    }

    pub(crate) fn duplicate_needs(span: Span, name: &str) -> Lint {
        Lint {
            span,
            message: format!("duplicate `needs` entry '{name}' (deduplicated)"),
            kind: LintKind::DuplicateNeeds,
        }
    }
}
