//! [`Lint`]: a plan-time warning that never blocks planning — as opposed to
//! a [`crate::plan::PlanError`], which does.
//!
//! Every case here is a warning rather than a hard error because GitHub
//! accepts the workflow and behaves in a specific (if sometimes surprising)
//! documented way: a dead `exclude` entry, a `needs.<job>.outputs.<name>`
//! reference to an output job `<job>` never declares (§4.3 — "GitHub
//! silently yields the empty string here, so hard-failing would reject
//! workflows GitHub accepts"), a matrix job's outputs being read by a
//! dependent (§4.3 — last-writer-wins across parallel legs).

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
