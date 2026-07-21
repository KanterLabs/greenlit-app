//! Conversions between `greenlit-workflow`'s parsed-YAML value types and
//! `greenlit-expr`'s runtime [`Value`] — the two crates deliberately mirror
//! each other's scalar kinds (see `greenlit_workflow::model::value::YamlScalar`'s
//! doc comment: "mirrors `greenlit-expr`'s expression `Value` kinds
//! deliberately ... so a value read from YAML and a value produced by
//! expression evaluation are shaped the same way once both crates meet in
//! `greenlit-engine`") — this module is exactly that meeting point.

use greenlit_expr::Value;
use greenlit_workflow::model::value::YamlScalar;
use greenlit_workflow::model::value::YamlValue;

/// Converts a parsed YAML scalar (already typed per GitHub's own YAML
/// scalar-typing rules) into an expression [`Value`] of the matching kind.
pub(crate) fn scalar_to_value(scalar: &YamlScalar) -> Value {
    match scalar {
        YamlScalar::Null => Value::Null,
        YamlScalar::Bool(b) => Value::Bool(*b),
        YamlScalar::Number(n) => Value::Number(*n),
        YamlScalar::String(s) => Value::String(s.clone()),
    }
}

/// Recursively converts a generic parsed `YamlValue` (used for matrix axis
/// values, `include`/`exclude` entries, and similar free-form structure)
/// into a [`Value`], for the plain-literal case only — an embedded
/// `${{ }}` expression is the caller's job to fold first (see
/// `crate::partial_eval`); this function panics-free-ly renders an
/// unresolved [`greenlit_workflow::model::value::ScalarOrExpr::Expression`]
/// as its raw source text (a [`Value::String`]) since matrix values that
/// still contain one at this point have already been rejected earlier in
/// the pipeline as a plan-time error (see `crate::matrix`).
pub(crate) fn yaml_value_to_value(v: &YamlValue) -> Value {
    match v {
        YamlValue::Scalar(scalar_or_expr) => match scalar_or_expr {
            greenlit_workflow::model::value::ScalarOrExpr::Literal(s) => scalar_to_value(s),
            greenlit_workflow::model::value::ScalarOrExpr::Expression(raw) => {
                Value::String(raw.clone())
            }
        },
        YamlValue::Sequence(items) => {
            Value::array(items.iter().map(|i| yaml_value_to_value(i)).collect())
        }
        YamlValue::Mapping(entries) => Value::object(
            entries
                .iter()
                .map(|(k, v)| (k.value.clone(), yaml_value_to_value(v)))
                .collect(),
        ),
    }
}
