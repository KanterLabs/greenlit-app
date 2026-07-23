//! Expression context values assembled for individual matrix legs.

use greenlit_expr::Value;

use crate::matrix::{MatrixLeg, MatrixValue, StrategyPlan};

pub(super) fn matrix_leg_value(leg: &MatrixLeg) -> Value {
    Value::object(
        leg.values
            .iter()
            .map(|(key, value)| (key.as_str().to_string(), matrix_value_to_expr_value(value)))
            .collect(),
    )
}

fn matrix_value_to_expr_value(value: &MatrixValue) -> Value {
    match value {
        MatrixValue::Null => Value::Null,
        MatrixValue::Bool(value) => Value::Bool(*value),
        MatrixValue::Number(value) => Value::Number(*value),
        MatrixValue::String(value) => Value::String(value.to_string()),
        MatrixValue::Sequence(items) => {
            Value::array(items.iter().map(matrix_value_to_expr_value).collect())
        }
        MatrixValue::Mapping(entries) => Value::object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), matrix_value_to_expr_value(value)))
                .collect(),
        ),
    }
}

/// GitHub's `strategy` context is populated independently for every matrix
/// job instance and is available wherever its context table permits it.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#strategy-context
pub(super) fn strategy_context_value(strategy: &StrategyPlan, index: usize) -> Value {
    // When `max-parallel` is omitted, GitHub maximizes concurrency and exposes
    // the effective matrix-job total through `strategy.max-parallel`.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#example-contents-of-the-strategy-context
    let job_total = strategy.legs().len() as f64;
    let fail_fast = strategy
        .fail_fast
        .as_static()
        .map_or(Value::Null, |value| Value::Bool(*value));
    let max_parallel = match strategy.max_parallel.as_ref() {
        None => Value::Number(job_total),
        Some(control) => control
            .as_static()
            .map_or(Value::Null, |value| Value::Number(value.get() as f64)),
    };
    Value::object(vec![
        ("fail-fast".to_string(), fail_fast),
        ("job-index".to_string(), Value::Number(index as f64)),
        ("job-total".to_string(), Value::Number(job_total)),
        ("max-parallel".to_string(), max_parallel),
    ])
}

/// Partial `strategy` object used while a prerequisite-produced matrix has
/// not supplied `job-index`/`job-total` yet. Explicit static controls remain
/// usable; omitted `max-parallel` stays unknown because its default is the
/// pending job total.
pub(super) fn deferred_strategy_context_value(strategy: &StrategyPlan) -> Value {
    let fail_fast = strategy
        .fail_fast
        .as_static()
        .map_or(Value::Null, |value| Value::Bool(*value));
    let max_parallel = strategy
        .max_parallel
        .as_ref()
        .and_then(crate::matrix::StrategyControl::as_static)
        .map_or(Value::Null, |value| Value::Number(value.get() as f64));
    Value::object(vec![
        ("fail-fast".to_string(), fail_fast),
        ("job-index".to_string(), Value::Null),
        ("job-total".to_string(), Value::Null),
        ("max-parallel".to_string(), max_parallel),
    ])
}
