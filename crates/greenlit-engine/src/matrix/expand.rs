//! The four-phase cartesian-product/exclude/include algorithm (design memo
//! §1.2), plus the conversions that let both an inline `strategy.matrix:`
//! mapping and a `${{ fromJSON(...) }}` expression's resulting object
//! converge on the same [`run_algorithm`].

use indexmap::IndexMap;

use greenlit_workflow::Span;
use greenlit_workflow::model::job::{Matrix, MatrixSource};
use greenlit_workflow::model::value::{ScalarOrExpr, YamlValue};

use crate::lints::Lint;
use crate::partial_eval::{FoldCtx, TemplateFold, fold_template};

use super::{LegOrigin, MatrixError, MatrixLeg, MatrixValue, ResolvedEntry, value_kind_name};

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
            return Err(MatrixError::DynamicMatrixNotSupported {
                span: text.span.clone(),
                defers_on: defers_on.into_iter().collect(),
            });
        }
    };

    match value {
        greenlit_expr::Value::Object(obj) => {
            let mut axes = Vec::new();
            let mut include = Vec::new();
            let mut exclude = Vec::new();
            for (k, v) in obj.iter() {
                if k.eq_ignore_ascii_case("include") {
                    include = value_to_matrix_entries(v, "include", &text.span)?;
                } else if k.eq_ignore_ascii_case("exclude") {
                    exclude = value_to_matrix_entries(v, "exclude", &text.span)?
                        .into_iter()
                        .map(|(_, entry, span)| (entry, span))
                        .collect();
                } else {
                    let greenlit_expr::Value::Array(items) = v else {
                        return Err(MatrixError::ExpressionFieldNotArray {
                            field: k.to_string(),
                            actual: value_kind_name(v),
                            span: text.span.clone(),
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
            run_algorithm(axes, include, exclude, cap, text.span.clone())
        }
        _ => Err(MatrixError::ExpressionNotMatrixShaped {
            span: text.span.clone(),
        }),
    }
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
        greenlit_expr::Value::String(s) => MatrixValue::String(s.clone()),
        greenlit_expr::Value::Array(items) => MatrixValue::Sequence(
            items
                .items()
                .iter()
                .map(expr_value_to_matrix_value)
                .collect(),
        ),
        greenlit_expr::Value::Object(obj) => MatrixValue::Mapping(
            obj.iter()
                .map(|(k, v)| (k.to_string(), expr_value_to_matrix_value(v)))
                .collect(),
        ),
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
            Ok(MatrixValue::Sequence(converted?))
        }
        YamlValue::Mapping(entries) => {
            let mut m = IndexMap::new();
            for (k, val) in entries {
                m.insert(k.value.clone(), convert_yaml_value(val, ctx)?);
            }
            Ok(MatrixValue::Mapping(m))
        }
    }
}

fn scalar_to_matrix_value(s: &greenlit_workflow::model::value::YamlScalar) -> MatrixValue {
    use greenlit_workflow::model::value::YamlScalar;
    match s {
        YamlScalar::Null => MatrixValue::Null,
        YamlScalar::Bool(b) => MatrixValue::Bool(*b),
        YamlScalar::Number(n) => MatrixValue::Number(*n),
        YamlScalar::String(s) => MatrixValue::String(s.clone()),
    }
}

/// The core four-phase algorithm (design memo §1.2), operating on
/// already-resolved [`MatrixValue`]s so both the inline-`strategy.matrix:`
/// path and the `${{ fromJSON(...) }}`-expression path converge here.
fn run_algorithm(
    axes: Vec<(String, Vec<MatrixValue>)>,
    include: Vec<(usize, ResolvedEntry, Span)>,
    exclude: Vec<(ResolvedEntry, Span)>,
    cap: usize,
    matrix_span: Span,
) -> Result<(Vec<MatrixLeg>, Vec<Lint>), MatrixError> {
    let original_keys: std::collections::HashSet<&str> =
        axes.iter().map(|(k, _)| k.as_str()).collect();

    // Phase 1: cartesian product, first axis outermost (varies slowest).
    // GitHub caps a matrix at 256 generated jobs. Compute the cardinality
    // with checked arithmetic and enforce the configured equivalent before
    // allocating any cartesian-product storage.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstrategymatrix
    let product_cardinality = if axes.is_empty() || axes.iter().any(|(_, values)| values.is_empty())
    {
        0
    } else {
        axes.iter()
            .try_fold(1_usize, |cardinality, (_, values)| {
                cardinality.checked_mul(values.len())
            })
            .ok_or_else(|| MatrixError::CardinalityOverflow {
                cap,
                span: matrix_span.clone(),
            })?
    };
    if product_cardinality > cap {
        return Err(MatrixError::TooManyLegs {
            count: product_cardinality,
            cap,
            span: matrix_span,
        });
    }

    let mut product: Vec<IndexMap<String, MatrixValue>> = if product_cardinality == 0 {
        Vec::new()
    } else {
        let mut combos: Vec<IndexMap<String, MatrixValue>> = vec![IndexMap::new()];
        for (key, values) in &axes {
            let next_cardinality = combos.len().checked_mul(values.len()).ok_or_else(|| {
                MatrixError::CardinalityOverflow {
                    cap,
                    span: matrix_span.clone(),
                }
            })?;
            let mut next = Vec::with_capacity(next_cardinality);
            for combo in &combos {
                for v in values {
                    let mut c = combo.clone();
                    c.insert(key.clone(), v.clone());
                    next.push(c);
                }
            }
            combos = next;
        }
        combos
    };

    // Phase 2: exclude, strictly before include.
    let mut lints = Vec::new();
    let mut surviving = vec![true; product.len()];
    for (entry, span) in &exclude {
        if entry.is_empty() {
            return Err(MatrixError::EmptyExcludeEntry { span: span.clone() });
        }
        let mut removed_any = false;
        for (i, combo) in product.iter().enumerate() {
            if surviving[i] && matches_exclude(entry, combo) {
                surviving[i] = false;
                removed_any = true;
            }
        }
        if !removed_any {
            lints.push(Lint::dead_exclude(span.clone()));
        }
    }
    product = product
        .into_iter()
        .enumerate()
        .filter(|(i, _)| surviving[*i])
        .map(|(_, c)| c)
        .collect();

    // Phase 3: include, sequential, fit-tested only against the surviving
    // product combinations (never against combos created by earlier
    // include entries).
    let mut legs: Vec<MatrixLeg> = product
        .into_iter()
        .map(|values| MatrixLeg {
            index: 0,
            values,
            origin: LegOrigin::Product,
        })
        .collect();
    let mut standalone: Vec<MatrixLeg> = Vec::new();
    for (entry_index, entry, _span) in &include {
        let mut fit_any = false;
        for leg in legs.iter_mut() {
            let fits = entry.iter().all(|(k, v)| {
                if original_keys.contains(k.as_str()) {
                    leg.values.get(k) == Some(v)
                } else {
                    true
                }
            });
            if fits {
                fit_any = true;
                for (k, v) in entry {
                    leg.values.insert(k.clone(), v.clone());
                }
            }
        }
        if !fit_any {
            let count = legs
                .len()
                .checked_add(standalone.len())
                .and_then(|current| current.checked_add(1))
                .ok_or_else(|| MatrixError::CardinalityOverflow {
                    cap,
                    span: matrix_span.clone(),
                })?;
            if count > cap {
                return Err(MatrixError::TooManyLegs {
                    count,
                    cap,
                    span: matrix_span,
                });
            }
            let mut values = IndexMap::new();
            for (k, v) in entry {
                values.insert(k.clone(), v.clone());
            }
            standalone.push(MatrixLeg {
                index: 0,
                values,
                origin: LegOrigin::Include {
                    entry_index: *entry_index,
                },
            });
        }
    }

    // Phase 4: result — surviving product combinations, then
    // include-created ones, both in their own order; assign final indices.
    legs.extend(standalone);
    for (i, leg) in legs.iter_mut().enumerate() {
        leg.index = i;
    }

    Ok((legs, lints))
}

fn matches_exclude(entry: &ResolvedEntry, combo: &IndexMap<String, MatrixValue>) -> bool {
    entry.iter().all(|(k, v)| combo.get(k) == Some(v))
}
