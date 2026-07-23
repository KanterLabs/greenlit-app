//! Ordered cartesian-product, exclude, and include processing for matrix
//! values that have already been resolved.

use greenlit_workflow::Span;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

use crate::lints::Lint;

use super::{LegOrigin, MatrixError, MatrixKey, MatrixLeg, MatrixValue, ResolvedEntry};

/// Computes and enforces a cartesian product from already-known axis
/// cardinalities without allocating any legs. An axis with no known values,
/// or an include-only matrix with no axes, has a zero minimum product.
pub(super) fn checked_product_cardinality(
    axis_cardinalities: &[usize],
    cap: usize,
    matrix_span: &Span,
) -> Result<usize, MatrixError> {
    let cardinality = if axis_cardinalities.is_empty() || axis_cardinalities.contains(&0) {
        0
    } else {
        axis_cardinalities
            .iter()
            .try_fold(1_usize, |product, cardinality| {
                product.checked_mul(*cardinality)
            })
            .ok_or_else(|| MatrixError::CardinalityOverflow {
                cap,
                span: matrix_span.clone(),
            })?
    };
    if cardinality > cap {
        return Err(MatrixError::TooManyLegs {
            count: cardinality,
            cap,
            span: matrix_span.clone(),
        });
    }
    Ok(cardinality)
}

/// Applies GitHub's documented product-then-exclude-then-include algorithm.
/// <https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/run-job-variations>
pub(super) fn run_algorithm(
    axes: Vec<(String, Vec<MatrixValue>)>,
    include: Vec<(usize, ResolvedEntry, Span)>,
    exclude: Vec<(ResolvedEntry, Span)>,
    cap: usize,
    matrix_span: Span,
) -> Result<(Vec<MatrixLeg>, Vec<Lint>), MatrixError> {
    let original_keys: HashSet<&str> = axes.iter().map(|(key, _)| key.as_str()).collect();
    let mut key_interner = HashMap::<String, MatrixKey>::new();

    // Phase 1: cartesian product, first axis outermost (varies slowest).
    // GitHub caps a matrix at 256 generated jobs. Compute the cardinality
    // with checked arithmetic and enforce the configured equivalent before
    // allocating any cartesian-product storage.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstrategymatrix
    let axis_cardinalities = axes
        .iter()
        .map(|(_, values)| values.len())
        .collect::<Vec<_>>();
    let product_cardinality = checked_product_cardinality(&axis_cardinalities, cap, &matrix_span)?;

    let mut product: Vec<IndexMap<MatrixKey, MatrixValue>> = if product_cardinality == 0 {
        Vec::new()
    } else {
        let mut combos: Vec<IndexMap<MatrixKey, MatrixValue>> = vec![IndexMap::new()];
        for (key, values) in &axes {
            let key = intern_key(&mut key_interner, key);
            let next_cardinality = combos.len().checked_mul(values.len()).ok_or_else(|| {
                MatrixError::CardinalityOverflow {
                    cap,
                    span: matrix_span.clone(),
                }
            })?;
            let mut next = Vec::with_capacity(next_cardinality);
            for combo in &combos {
                for value in values {
                    let mut expanded = combo.clone();
                    expanded.insert(key.clone(), value.clone());
                    next.push(expanded);
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
        for (index, combo) in product.iter().enumerate() {
            if surviving[index] && matches_exclude(entry, combo) {
                surviving[index] = false;
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
        .filter(|(index, _)| surviving[*index])
        .map(|(_, combo)| combo)
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
    let mut standalone = Vec::new();
    for (entry_index, entry, _span) in &include {
        let mut fit_any = false;
        for leg in &mut legs {
            let fits = entry.iter().all(|(key, value)| {
                if original_keys.contains(key.as_str()) {
                    leg.values.get(key.as_str()) == Some(value)
                } else {
                    true
                }
            });
            if fits {
                fit_any = true;
                for (key, value) in entry {
                    leg.values
                        .insert(intern_key(&mut key_interner, key), value.clone());
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
            for (key, value) in entry {
                values.insert(intern_key(&mut key_interner, key), value.clone());
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
    for (index, leg) in legs.iter_mut().enumerate() {
        leg.index = index;
    }

    Ok((legs, lints))
}

fn matches_exclude(entry: &ResolvedEntry, combo: &IndexMap<MatrixKey, MatrixValue>) -> bool {
    entry
        .iter()
        .all(|(key, value)| combo.get(key.as_str()) == Some(value))
}

fn intern_key(interner: &mut HashMap<String, MatrixKey>, key: &str) -> MatrixKey {
    if let Some(shared) = interner.get(key) {
        return shared.clone();
    }
    let shared = MatrixKey::from(key);
    interner.insert(key.to_string(), shared.clone());
    shared
}
