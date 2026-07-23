//! Runtime resolution of plan-time [`Planned`] values and [`Condition`]s.
//!
//! Phase 1 folds every context-sensitive field as far as the plan-time
//! contexts allow, leaving a residual [`greenlit_expr::Expr`] for anything
//! that needs runtime data. At execution time the driver rebuilds a full
//! [`Context`] (live `steps`/`needs`/`env`/`runner` values) and finishes the
//! residual here — never inventing a value earlier.

use greenlit_expr::value::{is_truthy, to_display_string, to_number};
use greenlit_expr::{Context, EvalError, evaluate};
use greenlit_workflow::Span;

use crate::condition::{Condition, PlannedCond};
use crate::planned::{Evaluation, Planned};

/// Resolves a planned string field (script body, `env:` value,
/// `working-directory`, `shell`, display name) against the live context.
pub fn resolve_string(planned: &Planned<String>, ctx: &Context) -> Result<String, EvalError> {
    match &planned.evaluation {
        Evaluation::Static(value) => Ok(value.clone()),
        Evaluation::Deferred(deferred) => {
            Ok(to_display_string(&evaluate(&deferred.residual, ctx)?))
        }
    }
}

/// Resolves a planned boolean workflow key (`continue-on-error`).
///
/// GitHub's typed boolean keys parse the evaluated result as a boolean
/// literal rather than applying general expression truthiness (under which
/// every non-empty string, including `"false"`, is truthy). A boolean value
/// is used directly; the strings `"true"`/`"false"` (case-insensitive) map to
/// their literal; anything else falls back to expression truthiness.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#fromjson>
pub fn resolve_bool(planned: &Planned<bool>, ctx: &Context) -> Result<bool, EvalError> {
    match &planned.evaluation {
        Evaluation::Static(value) => Ok(*value),
        Evaluation::Deferred(deferred) => {
            let value = evaluate(&deferred.residual, ctx)?;
            Ok(match &value {
                greenlit_expr::Value::Bool(bit) => *bit,
                greenlit_expr::Value::String(text) if text.eq_ignore_ascii_case("true") => true,
                greenlit_expr::Value::String(text) if text.eq_ignore_ascii_case("false") => false,
                other => is_truthy(other),
            })
        }
    }
}

/// A deferred `timeout-minutes` value either failed to evaluate or resolved
/// outside GitHub's supported range.
#[derive(Debug, thiserror::Error)]
pub enum TimeoutMinutesError {
    /// The residual expression itself failed to evaluate.
    #[error(transparent)]
    Eval(#[from] EvalError),
    /// The evaluated value is not a whole number of minutes from 1 through
    /// 360 (including non-finite values such as `Infinity`/`NaN`, which would
    /// otherwise panic `Duration::from_secs_f64` downstream).
    #[error(
        "{span}: timeout-minutes resolved to {value}, which is not a whole number of minutes from 1 through 360\n  fix: ensure the referenced value (e.g. a prior step's output) is an integer in that range"
    )]
    OutOfRange {
        /// Where `timeout-minutes` was authored.
        span: Span,
        /// The out-of-range evaluated value.
        value: f64,
    },
}

/// Resolves a planned `timeout-minutes` value to a number of minutes.
///
/// A statically-authored value was already validated at plan time
/// (`crate::planned::plan_scalar_number`: finite, integer, `1..=360`); a
/// deferred value (e.g. `${{ steps.x.outputs.minutes }}`) only becomes known
/// at runtime and must be validated the same way here — otherwise a
/// non-finite result such as `"Infinity"` would reach
/// `Duration::from_secs_f64`, which panics for non-finite input.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepstimeout-minutes>
pub fn resolve_minutes(planned: &Planned<f64>, ctx: &Context) -> Result<f64, TimeoutMinutesError> {
    match &planned.evaluation {
        Evaluation::Static(value) => Ok(*value),
        Evaluation::Deferred(deferred) => {
            let value = to_number(&evaluate(&deferred.residual, ctx)?);
            if value.is_finite() && value.fract() == 0.0 && (1.0..=360.0).contains(&value) {
                Ok(value)
            } else {
                Err(TimeoutMinutesError::OutOfRange {
                    span: planned.span.clone(),
                    value,
                })
            }
        }
    }
}

/// Resolves a planned `if:` condition to a boolean.
///
/// A statically-decided condition returns its folded value; a deferred one is
/// evaluated against `ctx` (whose [`greenlit_expr::RunStatus`] the caller has
/// set for the status-check functions) using expression truthiness, matching
/// how GitHub evaluates a conditional.
pub fn resolve_condition(condition: &Condition, ctx: &Context) -> Result<bool, EvalError> {
    match &condition.eval {
        PlannedCond::Static(value) => Ok(*value),
        PlannedCond::Deferred(deferred) => Ok(is_truthy(&evaluate(&deferred.residual, ctx)?)),
    }
}
