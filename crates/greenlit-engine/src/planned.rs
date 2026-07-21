//! Stable plan-time representation for authored values.
//!
//! Every context-sensitive field is either static now or carries the exact
//! residual expression and runtime dependencies needed to finish it later.

use serde::Serialize;

use greenlit_workflow::model::value::ScalarOrExpr;
use greenlit_workflow::{Span, Spanned};

use crate::condition::DeferredExpr;
use crate::convert::scalar_to_value;
use crate::json_shape::EvaluatedJson;
use crate::partial_eval::{
    FoldCtx, Folded, PartialEvalError, TemplateFold, fold_scalar_or_expr, fold_template,
    pretty_print,
};

/// One authored value together with its source location and plan-time state.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned<T> {
    /// Location of the authored value.
    pub span: Span,
    /// Verbatim expression/template text, or a typed YAML literal's
    /// canonical spelling (the workflow model does not retain literal
    /// lexemes).
    pub source: String,
    /// Static value or runtime residual.
    pub evaluation: Evaluation<T>,
}

/// Whether an authored value was decided at plan time.
#[derive(Debug, Clone, PartialEq)]
pub enum Evaluation<T> {
    /// Fully known before execution.
    Static(T),
    /// Requires runtime context and carries no invented value.
    Deferred(DeferredExpr),
}

impl<T: Serialize> Serialize for Planned<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match &self.evaluation {
            Evaluation::Static(value) => EvaluatedJson {
                span: self.span.to_string(),
                source: &self.source,
                evaluation: "static",
                value: Some(value),
                residual: None,
                defers_on: None,
            }
            .serialize(serializer),
            Evaluation::Deferred(deferred) => EvaluatedJson::<T> {
                span: self.span.to_string(),
                source: &self.source,
                evaluation: "deferred",
                value: None,
                residual: Some(&deferred.residual_text),
                defers_on: Some(&deferred.defers_on),
            }
            .serialize(serializer),
        }
    }
}

impl<T> Planned<T> {
    pub(crate) fn static_value(span: Span, source: String, value: T) -> Self {
        Self {
            span,
            source,
            evaluation: Evaluation::Static(value),
        }
    }
}

/// Plans a string-shaped YAML scalar or expression.
pub(crate) fn plan_scalar_string(
    authored: &Spanned<ScalarOrExpr>,
    ctx: &FoldCtx<'_>,
) -> Result<Planned<String>, PartialEvalError> {
    let source = scalar_source(&authored.value);
    let evaluation = match fold_scalar_or_expr(&authored.value, ctx)? {
        Folded::Value(value) => Evaluation::Static(greenlit_expr::value::to_display_string(&value)),
        Folded::Residual { expr, defers_on } => Evaluation::Deferred(DeferredExpr {
            residual_text: pretty_print(&expr),
            residual: expr,
            defers_on: defers_on.into_iter().collect(),
        }),
    };
    Ok(Planned {
        span: authored.span.clone(),
        source,
        evaluation,
    })
}

/// Plans a string template such as a script, shell, or display name.
pub(crate) fn plan_template_string(
    raw: &str,
    span: &Span,
    ctx: &FoldCtx<'_>,
) -> Result<Planned<String>, PartialEvalError> {
    let evaluation = match fold_template(raw, ctx)? {
        TemplateFold::Static(value) => {
            Evaluation::Static(greenlit_expr::value::to_display_string(&value))
        }
        TemplateFold::Deferred {
            residual,
            residual_text,
            defers_on,
        } => Evaluation::Deferred(DeferredExpr {
            residual,
            residual_text,
            defers_on: defers_on.into_iter().collect(),
        }),
    };
    Ok(Planned {
        span: span.clone(),
        source: raw.to_string(),
        evaluation,
    })
}

/// Plans a Boolean YAML scalar or expression.
///
/// GitHub's `fromJSON` example converts an environment string before using it
/// for `continue-on-error`, pinning that this typed field does not apply the
/// general expression truthiness coercion to strings.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#fromjson
pub(crate) fn plan_scalar_bool(
    authored: &Spanned<ScalarOrExpr>,
    ctx: &FoldCtx<'_>,
) -> Result<Planned<bool>, PartialEvalError> {
    let source = scalar_source(&authored.value);
    let evaluation = match fold_scalar_or_expr(&authored.value, ctx)? {
        Folded::Value(greenlit_expr::Value::Bool(value)) => Evaluation::Static(value),
        Folded::Value(_) => {
            return Err(PartialEvalError::InvalidStaticValue {
                expected: "a boolean",
            });
        }
        Folded::Residual { expr, defers_on } => Evaluation::Deferred(DeferredExpr {
            residual_text: pretty_print(&expr),
            residual: expr,
            defers_on: defers_on.into_iter().collect(),
        }),
    };
    Ok(Planned {
        span: authored.span.clone(),
        source,
        evaluation,
    })
}

/// Plans a step timeout as a whole number of minutes in GitHub's supported
/// `1..=360` range.
///
/// GitHub documents this field as an integer, rejects fractional values, and
/// caps it at 360 minutes.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepstimeout-minutes
pub(crate) fn plan_scalar_number(
    authored: &Spanned<ScalarOrExpr>,
    ctx: &FoldCtx<'_>,
) -> Result<Planned<f64>, PartialEvalError> {
    let source = scalar_source(&authored.value);
    let evaluation = match fold_scalar_or_expr(&authored.value, ctx)? {
        Folded::Value(greenlit_expr::Value::Number(number))
            if number.is_finite() && (1.0..=360.0).contains(&number) && number.fract() == 0.0 =>
        {
            Evaluation::Static(number)
        }
        Folded::Value(_) => {
            return Err(PartialEvalError::InvalidStaticValue {
                expected: "an integer from 1 through 360 minutes",
            });
        }
        Folded::Residual { expr, defers_on } => Evaluation::Deferred(DeferredExpr {
            residual_text: pretty_print(&expr),
            residual: expr,
            defers_on: defers_on.into_iter().collect(),
        }),
    };
    Ok(Planned {
        span: authored.span.clone(),
        source,
        evaluation,
    })
}

fn scalar_source(value: &ScalarOrExpr) -> String {
    match value {
        ScalarOrExpr::Literal(value) => {
            greenlit_expr::value::to_display_string(&scalar_to_value(value))
        }
        ScalarOrExpr::Expression(raw) => raw.clone(),
    }
}
