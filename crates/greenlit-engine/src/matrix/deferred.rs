//! Discovery and retention of runtime-dependent expressions embedded in a
//! matrix declaration.

use greenlit_workflow::model::job::MatrixSource;
use greenlit_workflow::model::value::{ScalarOrExpr, YamlValue};
use greenlit_workflow::{Span, Spanned};
use serde::Serialize;

use crate::partial_eval::{FoldCtx, TemplateFold, fold_template};

use super::MatrixError;

/// One runtime-dependent expression embedded in a deferred matrix source.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeferredMatrixExpression {
    /// Stable path within `strategy.matrix`, such as `matrix` for a
    /// whole-value expression or `matrix.os[0]` for an inline axis value.
    pub path: String,
    /// Location of the authored expression.
    #[serde(serialize_with = "crate::json_shape::serialize_span")]
    pub span: Span,
    /// Verbatim authored expression/template text.
    pub source: String,
    /// Partially folded expression text for runtime evaluation.
    pub residual: String,
    /// Sorted, deduplicated runtime dependencies.
    pub defers_on: Vec<crate::defer::DeferReason>,
}

pub(super) fn collect_deferred_matrix_expressions(
    source: &MatrixSource,
    ctx: &FoldCtx<'_>,
) -> Result<Vec<DeferredMatrixExpression>, MatrixError> {
    let mut expressions = Vec::new();
    match source {
        MatrixSource::Expression(value) => {
            collect_deferred_template("matrix".to_string(), value, ctx, &mut expressions)?;
        }
        MatrixSource::Inline(matrix) => {
            for (axis, values) in &matrix.axes {
                for (index, value) in values.iter().enumerate() {
                    collect_deferred_yaml(
                        format!("matrix.{}[{index}]", axis.value),
                        value,
                        ctx,
                        &mut expressions,
                    )?;
                }
            }
            for (index, entry) in matrix.include.iter().enumerate() {
                for (key, value) in &entry.value {
                    collect_deferred_yaml(
                        format!("matrix.include[{index}].{}", key.value),
                        value,
                        ctx,
                        &mut expressions,
                    )?;
                }
            }
            for (index, entry) in matrix.exclude.iter().enumerate() {
                for (key, value) in &entry.value {
                    collect_deferred_yaml(
                        format!("matrix.exclude[{index}].{}", key.value),
                        value,
                        ctx,
                        &mut expressions,
                    )?;
                }
            }
        }
    }
    Ok(expressions)
}

fn collect_deferred_yaml(
    path: String,
    value: &Spanned<YamlValue>,
    ctx: &FoldCtx<'_>,
    expressions: &mut Vec<DeferredMatrixExpression>,
) -> Result<(), MatrixError> {
    match &value.value {
        YamlValue::Scalar(ScalarOrExpr::Expression(source)) => collect_deferred_template(
            path,
            &Spanned::new(source.clone(), value.span.clone()),
            ctx,
            expressions,
        ),
        YamlValue::Sequence(values) => {
            for (index, nested) in values.iter().enumerate() {
                collect_deferred_yaml(format!("{path}[{index}]"), nested, ctx, expressions)?;
            }
            Ok(())
        }
        YamlValue::Mapping(entries) => {
            for (key, nested) in entries {
                collect_deferred_yaml(format!("{path}.{}", key.value), nested, ctx, expressions)?;
            }
            Ok(())
        }
        YamlValue::Scalar(ScalarOrExpr::Literal(_)) => Ok(()),
    }
}

fn collect_deferred_template(
    path: String,
    value: &Spanned<String>,
    ctx: &FoldCtx<'_>,
    expressions: &mut Vec<DeferredMatrixExpression>,
) -> Result<(), MatrixError> {
    let folded = fold_template(&value.value, ctx).map_err(|source| MatrixError::PartialEval {
        span: value.span.clone(),
        source,
    })?;
    if let TemplateFold::Deferred {
        residual: _,
        residual_text,
        defers_on,
    } = folded
    {
        expressions.push(DeferredMatrixExpression {
            path,
            span: value.span.clone(),
            source: value.value.clone(),
            residual: residual_text,
            defers_on: defers_on.into_iter().collect(),
        });
    }
    Ok(())
}
