//! [`Condition`]/[`PlannedCond`]: the plan-time result of partially
//! evaluating one `if:` expression (job- or step-level).
//!
//! This implements `PHASE-1-engine-core.md`'s static/deferred condition
//! planning and stable `--json` contract. The implicit `success()` gate
//! this crate's callers (`crate::plan`) attach alongside a [`Condition`] is
//! modeled structurally, not folded into the expression itself — see
//! `greenlit_expr::expr_calls_status_function`'s own doc comment, which this
//! crate's job/step planning consults directly.

use serde::Serialize;

use greenlit_expr::Expr;
use greenlit_expr::value::is_truthy;
use greenlit_workflow::Span;

use crate::defer::DeferReason;
use crate::json_shape::EvaluatedJson;
use crate::partial_eval::{FoldCtx, Folded, PartialEvalError, fold_expr, pretty_print};

/// One `if:` condition, fully resolved as far as plan time allows.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    /// Location of the authored `if:` value.
    pub span: Span,
    /// Verbatim authored text, including a `${{ }}` wrapper when present.
    pub source: String,
    /// The partial-evaluation result.
    pub eval: PlannedCond,
}

/// Either fully decided now, or left as a residual expression for runtime.
#[derive(Debug, Clone, PartialEq)]
pub enum PlannedCond {
    /// Fully decided at plan time. `false` means the node this condition
    /// belongs to is statically skipped.
    Static(bool),
    /// Could not be fully decided; see [`DeferredExpr`].
    Deferred(DeferredExpr),
}

/// The residual left after constant-folding a condition or output value as
/// far as possible. Shared verbatim between [`Condition`] and
/// `crate::outputs::PlannedValue` so all planned expressions use one
/// evaluator.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredExpr {
    /// The partially-folded AST — static subtrees already collapsed to
    /// literals.
    pub residual: Expr,
    /// Pretty-printed (or, for a template value, textually substituted)
    /// rendering of `residual`, shown to users.
    pub residual_text: String,
    /// Sorted, deduplicated: why this could not be decided now.
    pub defers_on: Vec<DeferReason>,
}

impl Serialize for Condition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match &self.eval {
            PlannedCond::Static(b) => EvaluatedJson {
                span: self.span.to_string(),
                source: &self.source,
                evaluation: "static",
                value: Some(b),
                residual: None,
                defers_on: None,
            }
            .serialize(serializer),
            PlannedCond::Deferred(d) => EvaluatedJson::<bool> {
                span: self.span.to_string(),
                source: &self.source,
                evaluation: "deferred",
                value: None,
                residual: Some(&d.residual_text),
                defers_on: Some(&d.defers_on),
            }
            .serialize(serializer),
        }
    }
}

/// Strips a whole-field `${{ ... }}` wrapper from an `if:` value, if the
/// entire (trimmed) field is exactly one such wrapper — GitHub allows `if:`
/// to be written either as a bare expression (`if: github.event_name ==
/// 'push'`) or fully wrapped (`if: ${{ github.event_name == 'push' }}`);
/// unlike output values or `env:` entries, an `if:` field is never a
/// multi-segment template mixing literal text with placeholders. GitHub
/// documents the optional wrapper at
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idif>.
fn strip_if_wrapper(raw: &str) -> &str {
    let trimmed = raw.trim();
    match trimmed
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
    {
        Some(inner) => inner.trim(),
        None => trimmed,
    }
}

/// Partially evaluates one `if:` field's raw text into a [`Condition`].
pub(crate) fn plan_condition(
    raw: &str,
    span: &Span,
    ctx: &FoldCtx<'_>,
) -> Result<Condition, PartialEvalError> {
    let expr = greenlit_expr::parse(strip_if_wrapper(raw))?;
    let eval = match fold_expr(&expr, ctx)? {
        // GitHub conditionals use expression truthiness, so a fully-static
        // condition always lands as `Static(bool)`.
        Folded::Value(v) => PlannedCond::Static(is_truthy(&v)),
        Folded::Residual { expr, defers_on } => PlannedCond::Deferred(DeferredExpr {
            residual_text: pretty_print(&expr),
            residual: expr,
            defers_on: defers_on.into_iter().collect(),
        }),
    };
    Ok(Condition {
        span: span.clone(),
        source: raw.to_string(),
        eval,
    })
}
