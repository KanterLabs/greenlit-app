//! Cartesian-product/exclude/include expansion, plus the conversions that
//! let both an inline `strategy.matrix:` mapping and a
//! `${{ fromJSON(...) }}` expression's resulting object converge on the
//! same expansion algorithm.

use indexmap::IndexMap;

use greenlit_workflow::Span;
use greenlit_workflow::model::job::{Matrix, MatrixSource};
use greenlit_workflow::model::value::{ScalarOrExpr, YamlValue};

use crate::lints::Lint;
use crate::partial_eval::{FoldCtx, TemplateFold, evaluate_template, fold_template};

use super::algorithm::{checked_product_cardinality, run_algorithm};
use super::{MatrixError, MatrixLeg, MatrixValue, ResolvedEntry, value_kind_name};

pub(super) fn expand_matrix_source(
    source: &MatrixSource,
    span: &Span,
    ctx: &FoldCtx<'_>,
    cap: usize,
) -> Result<(Vec<MatrixLeg>, Vec<Lint>), MatrixError> {
    match source {
        MatrixSource::Inline(matrix) => expand_inline_matrix(matrix, span, ctx, cap),
        MatrixSource::Expression(text) => expand_expression_matrix(text, ctx, cap),
    }
}

pub(super) fn expand_matrix_source_runtime(
    source: &MatrixSource,
    span: &Span,
    ctx: &greenlit_expr::Context,
    cap: usize,
) -> Result<(Vec<MatrixLeg>, Vec<Lint>), MatrixError> {
    match source {
        MatrixSource::Inline(matrix) => expand_inline_matrix_runtime(matrix, span, ctx, cap),
        MatrixSource::Expression(text) => {
            let value =
                evaluate_template(&text.value, ctx).map_err(|source| MatrixError::PartialEval {
                    span: text.span.clone(),
                    source,
                })?;
            expand_expression_value(value, &text.span, cap)
        }
    }
}

/// Validates every matrix fragment whose value is already known even when
/// another fragment keeps the matrix as a whole runtime-deferred. Deferral
/// is not permission to postpone an error Greenlit can prove now.
pub(super) fn validate_static_fragments(
    source: &MatrixSource,
    source_span: &Span,
    ctx: &FoldCtx<'_>,
    cap: usize,
) -> Result<(), MatrixError> {
    let MatrixSource::Inline(matrix) = source else {
        return Ok(());
    };
    let mut minimum_axis_cardinalities = Vec::with_capacity(matrix.axes.len());
    for (name, values) in &matrix.axes {
        let mut minimum_cardinality = 0_usize;
        for value in values {
            let known_values = match &value.value {
                YamlValue::Scalar(ScalarOrExpr::Expression(raw)) => {
                    match fold_template(raw, ctx).map_err(|source| MatrixError::PartialEval {
                        span: value.span.clone(),
                        source,
                    })? {
                        TemplateFold::Static(greenlit_expr::Value::Array(items)) => {
                            items.items().len()
                        }
                        TemplateFold::Deferred { .. } => 0,
                        TemplateFold::Static(actual) => {
                            return Err(MatrixError::ExpressionFieldNotArray {
                                field: name.value.clone(),
                                actual: value_kind_name(&actual),
                                span: value.span.clone(),
                            });
                        }
                    }
                }
                // A literal, sequence, or mapping is one matrix value even
                // if data nested inside it remains deferred. Only a
                // top-level expression yielding an array is spliced.
                _ => 1,
            };
            minimum_cardinality =
                minimum_cardinality
                    .checked_add(known_values)
                    .ok_or_else(|| MatrixError::CardinalityOverflow {
                        cap,
                        span: source_span.clone(),
                    })?;
        }
        minimum_axis_cardinalities.push(minimum_cardinality);
    }
    checked_product_cardinality(&minimum_axis_cardinalities, cap, source_span)?;
    if let Some(entry) = matrix.exclude.iter().find(|entry| entry.value.is_empty()) {
        return Err(MatrixError::EmptyExcludeEntry {
            span: entry.span.clone(),
        });
    }
    Ok(())
}

fn expand_inline_matrix(
    matrix: &Matrix,
    source_span: &Span,
    ctx: &FoldCtx<'_>,
    cap: usize,
) -> Result<(Vec<MatrixLeg>, Vec<Lint>), MatrixError> {
    let mut axes: Vec<(String, Vec<MatrixValue>)> = Vec::with_capacity(matrix.axes.len());
    for (name, values) in &matrix.axes {
        let mut converted = Vec::with_capacity(values.len());
        for v in values {
            if let YamlValue::Scalar(ScalarOrExpr::Expression(raw)) = &v.value {
                // GitHub defines each matrix variable as an array of values;
                // context-backed axes use the same shape and are spliced into
                // the axis only after resolving to an array.
                // https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/run-job-variations#using-contexts-to-create-matrices
                match fold_template(raw, ctx).map_err(|source| MatrixError::PartialEval {
                    span: v.span.clone(),
                    source,
                })? {
                    TemplateFold::Static(greenlit_expr::Value::Array(items)) => {
                        converted.extend(items.items().iter().map(expr_value_to_matrix_value))
                    }
                    TemplateFold::Static(value) => {
                        return Err(MatrixError::ExpressionFieldNotArray {
                            field: name.value.clone(),
                            actual: value_kind_name(&value),
                            span: v.span.clone(),
                        });
                    }
                    TemplateFold::Deferred { defers_on, .. } => {
                        return Err(MatrixError::ValueNotStatic {
                            span: v.span.clone(),
                            defers_on: defers_on.into_iter().collect(),
                        });
                    }
                }
            } else {
                converted.push(convert_yaml_value(v, ctx)?);
            }
        }
        axes.push((name.value.clone(), converted));
    }

    let mut include: Vec<(usize, ResolvedEntry, Span)> = Vec::with_capacity(matrix.include.len());
    for (i, entry) in matrix.include.iter().enumerate() {
        include.push((i, convert_matrix_entry(entry, ctx)?, entry.span.clone()));
    }

    let mut exclude: Vec<(ResolvedEntry, Span)> = Vec::with_capacity(matrix.exclude.len());
    for entry in &matrix.exclude {
        exclude.push((convert_matrix_entry(entry, ctx)?, entry.span.clone()));
    }

    run_algorithm(axes, include, exclude, cap, source_span.clone())
}

fn expand_expression_matrix(
    text: &greenlit_workflow::Spanned<String>,
    ctx: &FoldCtx<'_>,
    cap: usize,
) -> Result<(Vec<MatrixLeg>, Vec<Lint>), MatrixError> {
    let folded = fold_template(&text.value, ctx).map_err(|source| MatrixError::PartialEval {
        span: text.span.clone(),
        source,
    })?;
    let value = match folded {
        TemplateFold::Static(v) => v,
        TemplateFold::Deferred { defers_on, .. } => {
            return Err(MatrixError::ValueNotStatic {
                span: text.span.clone(),
                defers_on: defers_on.into_iter().collect(),
            });
        }
    };

    expand_expression_value(value, &text.span, cap)
}

fn expand_expression_value(
    value: greenlit_expr::Value,
    span: &Span,
    cap: usize,
) -> Result<(Vec<MatrixLeg>, Vec<Lint>), MatrixError> {
    match value {
        greenlit_expr::Value::Object(obj) => {
            let mut axes = Vec::new();
            let mut include = Vec::new();
            let mut exclude = Vec::new();
            for (k, v) in obj.iter() {
                if k.eq_ignore_ascii_case("include") {
                    include = value_to_matrix_entries(v, "include", span)?;
                } else if k.eq_ignore_ascii_case("exclude") {
                    exclude = value_to_matrix_entries(v, "exclude", span)?
                        .into_iter()
                        .map(|(_, entry, span)| (entry, span))
                        .collect();
                } else {
                    let greenlit_expr::Value::Array(items) = v else {
                        return Err(MatrixError::ExpressionFieldNotArray {
                            field: k.to_string(),
                            actual: value_kind_name(v),
                            span: span.clone(),
                        });
                    };
                    axes.push((
                        k.to_string(),
                        items
                            .items()
                            .iter()
                            .map(expr_value_to_matrix_value)
                            .collect(),
                    ));
                }
            }
            run_algorithm(axes, include, exclude, cap, span.clone())
        }
        _ => Err(MatrixError::ExpressionNotMatrixShaped { span: span.clone() }),
    }
}

fn expand_inline_matrix_runtime(
    matrix: &Matrix,
    source_span: &Span,
    ctx: &greenlit_expr::Context,
    cap: usize,
) -> Result<(Vec<MatrixLeg>, Vec<Lint>), MatrixError> {
    let mut axes = Vec::with_capacity(matrix.axes.len());
    for (name, values) in &matrix.axes {
        let mut converted = Vec::with_capacity(values.len());
        for value in values {
            if let YamlValue::Scalar(ScalarOrExpr::Expression(raw)) = &value.value {
                let evaluated =
                    evaluate_template(raw, ctx).map_err(|source| MatrixError::PartialEval {
                        span: value.span.clone(),
                        source,
                    })?;
                match evaluated {
                    greenlit_expr::Value::Array(items) => {
                        converted.extend(items.items().iter().map(expr_value_to_matrix_value))
                    }
                    actual => {
                        return Err(MatrixError::ExpressionFieldNotArray {
                            field: name.value.clone(),
                            actual: value_kind_name(&actual),
                            span: value.span.clone(),
                        });
                    }
                }
            } else {
                converted.push(convert_yaml_value_runtime(value, ctx)?);
            }
        }
        axes.push((name.value.clone(), converted));
    }

    let include = matrix
        .include
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            Ok((
                index,
                convert_matrix_entry_runtime(entry, ctx)?,
                entry.span.clone(),
            ))
        })
        .collect::<Result<Vec<_>, MatrixError>>()?;
    let exclude = matrix
        .exclude
        .iter()
        .map(|entry| {
            Ok((
                convert_matrix_entry_runtime(entry, ctx)?,
                entry.span.clone(),
            ))
        })
        .collect::<Result<Vec<_>, MatrixError>>()?;
    run_algorithm(axes, include, exclude, cap, source_span.clone())
}

/// `fromJSON(...)`-sourced `include`/`exclude` entries carry no per-entry
/// span (they come from evaluated data, not parsed YAML nodes); every
/// entry is attributed to the enclosing `strategy.matrix: ${{ ... }}`
/// field's own span instead.
fn value_to_matrix_entries(
    v: &greenlit_expr::Value,
    field: &'static str,
    fallback: &Span,
) -> Result<Vec<(usize, ResolvedEntry, Span)>, MatrixError> {
    let greenlit_expr::Value::Array(items) = v else {
        return Err(MatrixError::ExpressionFieldNotArray {
            field: field.to_string(),
            actual: value_kind_name(v),
            span: fallback.clone(),
        });
    };
    items
        .items()
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let greenlit_expr::Value::Object(obj) = item else {
                return Err(MatrixError::ExpressionEntryNotObject {
                    field,
                    index,
                    actual: value_kind_name(item),
                    span: fallback.clone(),
                });
            };
            let entry = obj
                .iter()
                .map(|(key, value)| (key.to_string(), expr_value_to_matrix_value(value)))
                .collect();
            Ok((index, entry, fallback.clone()))
        })
        .collect()
}

fn expr_value_to_matrix_value(v: &greenlit_expr::Value) -> MatrixValue {
    match v {
        greenlit_expr::Value::Null => MatrixValue::Null,
        greenlit_expr::Value::Bool(b) => MatrixValue::Bool(*b),
        greenlit_expr::Value::Number(n) => MatrixValue::Number(*n),
        greenlit_expr::Value::String(s) => MatrixValue::String(s.clone().into()),
        greenlit_expr::Value::Array(items) => MatrixValue::Sequence(
            items
                .items()
                .iter()
                .map(expr_value_to_matrix_value)
                .collect::<Vec<_>>()
                .into(),
        ),
        greenlit_expr::Value::Object(obj) => MatrixValue::Mapping(std::sync::Arc::new(
            obj.iter()
                .map(|(k, v)| (k.to_string(), expr_value_to_matrix_value(v)))
                .collect(),
        )),
    }
}

fn convert_matrix_entry(
    entry: &greenlit_workflow::Spanned<greenlit_workflow::model::job::MatrixEntry>,
    ctx: &FoldCtx<'_>,
) -> Result<ResolvedEntry, MatrixError> {
    entry
        .value
        .iter()
        .map(|(k, v)| Ok((k.value.clone(), convert_yaml_value(v, ctx)?)))
        .collect()
}

fn convert_matrix_entry_runtime(
    entry: &greenlit_workflow::Spanned<greenlit_workflow::model::job::MatrixEntry>,
    ctx: &greenlit_expr::Context,
) -> Result<ResolvedEntry, MatrixError> {
    entry
        .value
        .iter()
        .map(|(key, value)| Ok((key.value.clone(), convert_yaml_value_runtime(value, ctx)?)))
        .collect()
}

fn convert_yaml_value_runtime(
    value: &greenlit_workflow::Spanned<YamlValue>,
    ctx: &greenlit_expr::Context,
) -> Result<MatrixValue, MatrixError> {
    match &value.value {
        YamlValue::Scalar(ScalarOrExpr::Literal(scalar)) => Ok(scalar_to_matrix_value(scalar)),
        YamlValue::Scalar(ScalarOrExpr::Expression(raw)) => evaluate_template(raw, ctx)
            .map(|evaluated| expr_value_to_matrix_value(&evaluated))
            .map_err(|source| MatrixError::PartialEval {
                span: value.span.clone(),
                source,
            }),
        YamlValue::Sequence(items) => Ok(MatrixValue::Sequence(
            items
                .iter()
                .map(|item| convert_yaml_value_runtime(item, ctx))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        )),
        YamlValue::Mapping(entries) => Ok(MatrixValue::Mapping(std::sync::Arc::new(
            entries
                .iter()
                .map(|(key, nested)| {
                    Ok((key.value.clone(), convert_yaml_value_runtime(nested, ctx)?))
                })
                .collect::<Result<IndexMap<_, _>, MatrixError>>()?,
        ))),
    }
}

fn convert_yaml_value(
    v: &greenlit_workflow::Spanned<YamlValue>,
    ctx: &FoldCtx<'_>,
) -> Result<MatrixValue, MatrixError> {
    match &v.value {
        YamlValue::Scalar(ScalarOrExpr::Literal(scalar)) => Ok(scalar_to_matrix_value(scalar)),
        YamlValue::Scalar(ScalarOrExpr::Expression(raw)) => {
            match fold_template(raw, ctx).map_err(|source| MatrixError::PartialEval {
                span: v.span.clone(),
                source,
            })? {
                TemplateFold::Static(value) => Ok(expr_value_to_matrix_value(&value)),
                TemplateFold::Deferred { defers_on, .. } => Err(MatrixError::ValueNotStatic {
                    span: v.span.clone(),
                    defers_on: defers_on.into_iter().collect(),
                }),
            }
        }
        YamlValue::Sequence(items) => {
            let converted: Result<Vec<_>, _> =
                items.iter().map(|i| convert_yaml_value(i, ctx)).collect();
            Ok(MatrixValue::Sequence(converted?.into()))
        }
        YamlValue::Mapping(entries) => {
            let mut m = IndexMap::new();
            for (k, val) in entries {
                m.insert(k.value.clone(), convert_yaml_value(val, ctx)?);
            }
            Ok(MatrixValue::Mapping(std::sync::Arc::new(m)))
        }
    }
}

fn scalar_to_matrix_value(s: &greenlit_workflow::model::value::YamlScalar) -> MatrixValue {
    use greenlit_workflow::model::value::YamlScalar;
    match s {
        YamlScalar::Null => MatrixValue::Null,
        YamlScalar::Bool(b) => MatrixValue::Bool(*b),
        YamlScalar::Number(n) => MatrixValue::Number(*n),
        YamlScalar::String(s) => MatrixValue::String(s.clone().into()),
    }
}
