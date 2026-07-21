//! `runs-on:` → [`RunnerImage`]: supported-runner validation.
//!
//! Source: `greenlit-v0-spec.md` "Tech" — "Supported runner labels:
//! `ubuntu-latest`, `ubuntu-24.04`, `ubuntu-22.04`. Every other label is
//! recognized and rejected during planning with the supported list and
//! source location" — cross-referenced against GitHub's
//! [hosted-runner label table](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job),
//! which lists `ubuntu-latest`/`ubuntu-24.04`/`ubuntu-22.04` (x64) among
//! Linux runners, plus ARM variants, `windows-*`, `macos-*`, and the
//! larger-runner/self-hosted/custom-group forms this crate rejects.
//! `ubuntu-latest` resolves to 24.04 for v0 (`greenlit-v0-spec.md`
//! "v0 scope (in)").

use greenlit_expr::value::to_display_string;
use greenlit_workflow::Span;
use greenlit_workflow::Spanned;
use greenlit_workflow::model::job::RunsOn;

use crate::partial_eval::{FoldCtx, PartialEvalError, TemplateFold, fold_template};

/// The stable x64 Ubuntu runner images v0 supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RunnerImage {
    /// `ubuntu-24.04` (also what `ubuntu-latest` resolves to in v0).
    Ubuntu2404,
    /// `ubuntu-22.04`.
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

/// Every `runs-on:` label v0 accepts, as written in the workflow file —
/// `ubuntu-latest` included, even though it resolves to
/// [`RunnerImage::Ubuntu2404`], because this is the *accepted-input* list
/// shown in error messages, not the resolved-image list.
pub const SUPPORTED_RUNNER_LABELS: [&str; 3] = ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04"];

/// Resolves a bare label string (already known statically) to a
/// [`RunnerImage`], or `None` if unsupported.
#[must_use]
pub fn resolve_runner_label(label: &str) -> Option<RunnerImage> {
    match label {
        "ubuntu-latest" | "ubuntu-24.04" => Some(RunnerImage::Ubuntu2404),
        "ubuntu-22.04" => Some(RunnerImage::Ubuntu2204),
        _ => None,
    }
}

/// The [`SUPPORTED_RUNNER_LABELS`] list, pre-joined for error messages —
/// kept as a literal (rather than computed at the `#[error(...)]` site) so
/// the message text is unambiguous `thiserror` syntax; the unit test below
/// pins it against [`SUPPORTED_RUNNER_LABELS`] itself so the two can never
/// silently drift.
const SUPPORTED_RUNNER_LABELS_DISPLAY: &str = "ubuntu-latest, ubuntu-24.04, ubuntu-22.04";

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
    /// `runs-on:` used a construct v0 does not accept at all:
    /// `runs-on: [...]` (a self-hosted AND-matched label list) or
    /// `runs-on: { group: ..., labels: [...] }` (a custom runner group) —
    /// both are documented `Out (v0)` (`greenlit-v0-spec.md`).
    #[error(
        "{span}: {construct} is not supported in v0 — v0 supports only: {SUPPORTED_RUNNER_LABELS_DISPLAY}"
    )]
    UnsupportedConstruct {
        /// Where `runs-on:` appears.
        span: Span,
        /// A short description of the construct.
        construct: &'static str,
    },
    /// `runs-on: ${{ ... }}` depends on data not statically knowable at
    /// plan time (e.g. `needs.<id>.outputs.<name>`) — the runner image must
    /// be known before any container work happens, so this is a hard v0
    /// error rather than a deferral.
    #[error("{span}: runs-on depends on data not available at plan time")]
    NotStatic {
        /// Where the `runs-on:` value appears.
        span: Span,
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
}

/// Resolves `runs-on:` for one job (or one matrix leg — `ctx.roots.matrix`
/// carries that leg's `matrix` value) into a [`RunnerImage`].
///
/// A literal label is checked directly; `runs-on: ${{ matrix.os }}` (an
/// extremely common pattern) is supported by folding the whole-field
/// expression against `ctx` and checking the resulting string — but only
/// when it is statically resolvable *now*: unlike `if:`/outputs, a runner
/// label can never be left "runtime-deferred", since Greenlit must know
/// which image to use before any container work starts. Anything else
/// (multiple interpolations, literal text mixed in, or a reference to
/// non-static data) is rejected rather than guessed at.
pub(crate) fn resolve_runs_on(
    runs_on: &Spanned<RunsOn>,
    ctx: &FoldCtx<'_>,
) -> Result<RunnerImage, RunnerError> {
    match &runs_on.value {
        RunsOn::Label(s) => resolve_label_text(s, ctx),
        RunsOn::Labels(_) => Err(RunnerError::UnsupportedConstruct {
            span: runs_on.span.clone(),
            construct: "a self-hosted `runs-on: [...]` label list",
        }),
        RunsOn::Group { .. } => Err(RunnerError::UnsupportedConstruct {
            span: runs_on.span.clone(),
            construct: "a custom `runs-on: { group: ... }` runner group",
        }),
    }
}

fn resolve_label_text(s: &Spanned<String>, ctx: &FoldCtx<'_>) -> Result<RunnerImage, RunnerError> {
    if !s.value.contains("${{") {
        return lookup_or_error(&s.value, &s.span);
    }
    match fold_template(&s.value, ctx) {
        Ok(TemplateFold::Static(v)) => lookup_or_error(&to_display_string(&v), &s.span),
        Ok(TemplateFold::Deferred { .. }) => Err(RunnerError::NotStatic {
            span: s.span.clone(),
        }),
        Err(source) => Err(RunnerError::PartialEval {
            span: s.span.clone(),
            source,
        }),
    }
}

fn lookup_or_error(label: &str, span: &Span) -> Result<RunnerImage, RunnerError> {
    resolve_runner_label(label).ok_or_else(|| RunnerError::Unsupported {
        span: span.clone(),
        label: label.to_string(),
    })
}
