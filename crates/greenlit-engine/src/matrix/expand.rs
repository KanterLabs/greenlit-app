//! The four-phase cartesian-product/exclude/include algorithm (design memo
//! §1.2), plus the conversions that let both an inline `strategy.matrix:`
//! mapping and a `${{ fromJSON(...) }}` expression's resulting object/array
//! converge on the same [`run_algorithm`].

use indexmap::IndexMap;

use greenlit_workflow::Span;
use greenlit_workflow::model::job::{Matrix, MatrixSource};
use greenlit_workflow::model::value::{ScalarOrExpr, YamlValue};

use crate::lints::Lint;
use crate::partial_eval::{FoldCtx, TemplateFold, fold_template};

use super::{LegOrigin, MatrixError, MatrixLeg, MatrixValue, ResolvedEntry};

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
            converted.push(convert_yaml_value(v, ctx)?);
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

    run_algorithm(
        axes,
        include,
        exclude,
        cap,
        matrix_span(matrix, source_span),
    )
}

/// The whole-document span doesn't carry a dedicated `strategy.matrix`
/// span on [`Matrix`] itself; the cap error still needs *some* location, so
/// this falls back to the first axis/include/exclude entry's span found, or
/// (if the matrix is entirely empty) the enclosing `strategy.matrix:`
/// node's own span.
fn matrix_span(matrix: &Matrix, fallback: &Span) -> Span {
    matrix
        .axes
        .first()
        .map(|(k, _)| k.span.clone())
        .or_else(|| matrix.include.first().map(|e| e.span.clone()))
        .or_else(|| matrix.exclude.first().map(|e| e.span.clone()))
        .unwrap_or_else(|| fallback.clone())
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
        greenlit_expr::Value::Array(items) => {
            let include: Vec<(usize, ResolvedEntry, Span)> = items
                .items()
                .iter()
                .enumerate()
                .map(|(i, item)| (i, object_value_to_entry(item), text.span.clone()))
                .collect();
            run_algorithm(Vec::new(), include, Vec::new(), cap, text.span.clone())
        }
        greenlit_expr::Value::Object(obj) => {
            let mut axes = Vec::new();
            let mut include = Vec::new();
            let mut exclude = Vec::new();
            for (k, v) in obj.iter() {
                if k.eq_ignore_ascii_case("include") {
                    include = value_to_matrix_entries(v, &text.span);
                } else if k.eq_ignore_ascii_case("exclude") {
                    exclude = value_to_matrix_entries(v, &text.span)
                        .into_iter()
                        .map(|(_, entry, span)| (entry, span))
                        .collect();
                } else if let greenlit_expr::Value::Array(items) = v {
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

fn object_value_to_entry(v: &greenlit_expr::Value) -> ResolvedEntry {
    match v {
        greenlit_expr::Value::Object(obj) => obj
            .iter()
            .map(|(k, v)| (k.to_string(), expr_value_to_matrix_value(v)))
            .collect(),
        other => vec![("value".to_string(), expr_value_to_matrix_value(other))],
    }
}

/// `fromJSON(...)`-sourced `include`/`exclude` entries carry no per-entry
/// span (they come from evaluated data, not parsed YAML nodes); every
/// entry is attributed to the enclosing `strategy.matrix: ${{ ... }}`
/// field's own span instead.
fn value_to_matrix_entries(
    v: &greenlit_expr::Value,
    fallback: &Span,
) -> Vec<(usize, ResolvedEntry, Span)> {
    match v {
        greenlit_expr::Value::Array(items) => items
            .items()
            .iter()
            .enumerate()
            .map(|(i, item)| (i, object_value_to_entry(item), fallback.clone()))
            .collect(),
        _ => Vec::new(),
    }
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
    let mut product: Vec<IndexMap<String, MatrixValue>> = if axes.is_empty() {
        Vec::new()
    } else {
        let mut combos: Vec<IndexMap<String, MatrixValue>> = vec![IndexMap::new()];
        for (key, values) in &axes {
            let mut next = Vec::with_capacity(combos.len() * values.len());
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

    if legs.len() > cap {
        return Err(MatrixError::TooManyLegs {
            count: legs.len(),
            cap,
            span: matrix_span,
        });
    }

    Ok((legs, lints))
}

fn matches_exclude(entry: &ResolvedEntry, combo: &IndexMap<String, MatrixValue>) -> bool {
    entry.iter().all(|(k, v)| combo.get(k) == Some(v))
}
