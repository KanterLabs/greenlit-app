//! `runs-on:` → [`RunnerImage`]: supported-runner validation.
//!
//! Source: `greenlit-v0-spec.md` "Tech" — "Supported runner labels:
//! `ubuntu-latest`, `ubuntu-24.04`, `ubuntu-22.04`, and the KanterLabs
//! `homelab` and `homelab-heavy` Ubuntu runners. Every other label is
//! recognized and rejected during planning with the supported list and
//! source location" — cross-referenced against GitHub's
//! [hosted-runner label table](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job),
//! which lists `ubuntu-latest`/`ubuntu-24.04`/`ubuntu-22.04` (x64) among
//! Linux runners, plus ARM variants, `windows-*`, `macos-*`, and the
//! larger-runner/self-hosted/custom-group forms this crate rejects.
//! `ubuntu-latest` resolves to 24.04 for v0 (`greenlit-v0-spec.md`
//! "v0 scope (in)").

use greenlit_workflow::Span;
use greenlit_workflow::Spanned;
use greenlit_workflow::model::job::RunsOn;

use crate::partial_eval::{FoldCtx, PartialEvalError};
use crate::planned::{Evaluation, Planned, plan_template_string};

/// The stable x64 Ubuntu runner images v0 supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RunnerImage {
    /// `ubuntu-24.04` (also what `ubuntu-latest` resolves to in v0).
    #[serde(rename = "ubuntu-24.04")]
    Ubuntu2404,
    /// `ubuntu-22.04`.
    #[serde(rename = "ubuntu-22.04")]
    Ubuntu2204,
}

impl RunnerImage {
    /// The resolved runner-image identifier this plan carries — Phase 2
    /// maps this to an actual container image tag; no image handling
    /// exists in this phase (`PHASE-1-engine-core.md`: "No containers,
    /// Docker access, or network calls in this phase").
    #[must_use]
    pub fn image_identifier(&self) -> &'static str {
        match self {
            RunnerImage::Ubuntu2404 => "ubuntu-24.04",
            RunnerImage::Ubuntu2204 => "ubuntu-22.04",
        }
    }
}

/// A runner selection folded as far as the currently available contexts
/// permit.
///
/// Static selections have already passed the v0 label allowlist and carry
/// their resolved image. Deferred selections retain the authored expression
/// and exact runtime dependencies; execution must evaluate them before it
/// provisions a container, then validate the resulting label with
/// [`resolve_runner_label`].
pub type RunnerPlan = Planned<RunnerImage>;

/// Every `runs-on:` label v0 accepts, as written in the workflow file —
/// `ubuntu-latest` included, even though it resolves to
/// [`RunnerImage::Ubuntu2404`], because this is the *accepted-input* list
/// shown in error messages, not the resolved-image list.
pub const SUPPORTED_RUNNER_LABELS: [&str; 5] = [
    "ubuntu-latest",
    "ubuntu-24.04",
    "ubuntu-22.04",
    "homelab",
    "homelab-heavy",
];

/// Resolves a bare label string (already known statically) to a
/// [`RunnerImage`], or `None` if unsupported.
#[must_use]
pub fn resolve_runner_label(label: &str) -> Option<RunnerImage> {
    match label {
        "ubuntu-latest" | "ubuntu-24.04" | "homelab" | "homelab-heavy" => {
            Some(RunnerImage::Ubuntu2404)
        }
        "ubuntu-22.04" => Some(RunnerImage::Ubuntu2204),
        _ => None,
    }
}

/// The [`SUPPORTED_RUNNER_LABELS`] list, pre-joined for error messages —
/// kept as a literal (rather than computed at the `#[error(...)]` site) so
/// the message text is unambiguous `thiserror` syntax. The supported-runner
/// rejection integration test pins the displayed list so it cannot silently
/// drift from [`SUPPORTED_RUNNER_LABELS`].
const SUPPORTED_RUNNER_LABELS_DISPLAY: &str =
    "ubuntu-latest, ubuntu-24.04, ubuntu-22.04, homelab, homelab-heavy";

/// Everything that can go wrong resolving `runs-on:`.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The label (or the value a `${{ matrix.* }}` label resolved to) is
    /// not one of [`SUPPORTED_RUNNER_LABELS`].
    #[error(
        "{span}: unsupported runner '{label}' — v0 supports only: {SUPPORTED_RUNNER_LABELS_DISPLAY}"
    )]
    Unsupported {
        /// Where the `runs-on:` value appears.
        span: Span,
        /// The unsupported label text.
        label: String,
    },
    /// `runs-on:` used a construct v0 does not accept: a multi-label
    /// AND-matched list (self-hosted selection) or a custom runner group.
    #[error(
        "{span}: {construct} is not supported in v0 — v0 supports only: {SUPPORTED_RUNNER_LABELS_DISPLAY}"
    )]
    UnsupportedConstruct {
        /// Where `runs-on:` appears.
        span: Span,
        /// A short description of the construct.
        construct: &'static str,
    },
    /// An embedded expression failed to parse or evaluate.
    #[error("{span}: {source}")]
    PartialEval {
        /// Where the `runs-on:` value appears.
        span: Span,
        /// The underlying failure.
        #[source]
        source: PartialEvalError,
    },
    /// A deferred runner label failed when its runtime dependencies existed.
    #[error("{span}: could not evaluate the runtime runner label: {source}")]
    RuntimeEval {
        /// Where the `runs-on:` value appears.
        span: Span,
        /// The runtime expression failure.
        #[source]
        source: greenlit_expr::EvalError,
    },
}

/// Finish a deferred runner selection against a live job context.
pub fn materialize_runner(
    planned: &RunnerPlan,
    ctx: &greenlit_expr::Context,
) -> Result<RunnerImage, RunnerError> {
    match &planned.evaluation {
        Evaluation::Static(image) => Ok(*image),
        Evaluation::Deferred(deferred) => {
            let value = greenlit_expr::evaluate(&deferred.residual, ctx).map_err(|source| {
                RunnerError::RuntimeEval {
                    span: planned.span.clone(),
                    source,
                }
            })?;
            let label = greenlit_expr::value::to_display_string(&value);
            lookup_or_error(&label, &planned.span)
        }
    }
}

/// Resolves `runs-on:` for one job (or one matrix leg — `ctx.roots.matrix`
/// carries that leg's `matrix` value) into a [`RunnerPlan`].
///
/// A literal label is checked directly; `runs-on: ${{ matrix.os }}` (an
/// extremely common pattern) is supported by folding the whole-field
/// expression against `ctx` and checking the resulting string. A label that
/// depends on a runtime-only context (most commonly a direct dependency's
/// output) remains explicitly deferred without an invented image; execution
/// must finish that evaluation before any container work starts.
pub(crate) fn resolve_runs_on(
    runs_on: &Spanned<RunsOn>,
    ctx: &FoldCtx<'_>,
) -> Result<RunnerPlan, RunnerError> {
    match &runs_on.value {
        RunsOn::Label(s) => resolve_label_text(s, ctx),
        RunsOn::Labels(labels) if labels.len() == 1 => resolve_label_text(&labels[0], ctx),
        RunsOn::Labels(_) => Err(RunnerError::UnsupportedConstruct {
            span: runs_on.span.clone(),
            construct: "a multi-label `runs-on: [...]` self-hosted selection",
        }),
        RunsOn::Group { .. } => Err(RunnerError::UnsupportedConstruct {
            span: runs_on.span.clone(),
            construct: "a custom `runs-on: { group: ... }` runner group",
        }),
    }
}

fn resolve_label_text(s: &Spanned<String>, ctx: &FoldCtx<'_>) -> Result<RunnerPlan, RunnerError> {
    let planned = plan_template_string(&s.value, &s.span, ctx).map_err(|source| {
        RunnerError::PartialEval {
            span: s.span.clone(),
            source,
        }
    })?;
    let evaluation = match planned.evaluation {
        Evaluation::Static(label) => Evaluation::Static(lookup_or_error(&label, &s.span)?),
        Evaluation::Deferred(deferred) => Evaluation::Deferred(deferred),
    };
    Ok(Planned {
        span: planned.span,
        source: planned.source,
        evaluation,
    })
}

fn lookup_or_error(label: &str, span: &Span) -> Result<RunnerImage, RunnerError> {
    resolve_runner_label(label).ok_or_else(|| RunnerError::Unsupported {
        span: span.clone(),
        label: label.to_string(),
    })
}
