//! Expression context values assembled for individual matrix legs.

use greenlit_expr::Value;

use crate::matrix::{MatrixLeg, MatrixValue, StrategyPlan};

pub(super) fn matrix_leg_value(leg: &MatrixLeg) -> Value {
    Value::object(
        leg.values
            .iter()
            .map(|(key, value)| (key.clone(), matrix_value_to_expr_value(value)))
            .collect(),
    )
}

fn matrix_value_to_expr_value(value: &MatrixValue) -> Value {
    match value {
        MatrixValue::Null => Value::Null,
        MatrixValue::Bool(value) => Value::Bool(*value),
        MatrixValue::Number(value) => Value::Number(*value),
        MatrixValue::String(value) => Value::String(value.clone()),
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
    let max_parallel = strategy
        .max_parallel
        .map_or(strategy.legs.len() as f64, |value| value.get() as f64);
    Value::object(vec![
        ("fail-fast".to_string(), Value::Bool(strategy.fail_fast)),
        ("job-index".to_string(), Value::Number(index as f64)),
        (
            "job-total".to_string(),
            Value::Number(strategy.legs.len() as f64),
        ),
        ("max-parallel".to_string(), Value::Number(max_parallel)),
    ])
}
