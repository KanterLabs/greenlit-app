//! Converts a [`greenlit_expr::Value`] into a real [`serde_json::Value`], for
//! writing the synthetic `github.event` payload to `$GITHUB_EVENT_PATH`
//! (`crate::executor::cmdfiles::write_event_file`).
//!
//! `greenlit_expr`'s own expression-language `toJSON()` renders GitHub's
//! *expression* JSON, which is deliberately not always valid JSON — a
//! non-finite number renders as a bare `Infinity`/`NaN` token, matching the
//! real runner's own `ExpressionUtility.StringifyValue`
//! (`actions/runner` v2.336.0, pinned release). A file a step's own `jq`/
//! `JSON.parse`/`json.load` reads has to be real JSON, so this is a
//! separate, honest conversion rather than a reuse of that renderer.

use greenlit_expr::Value;

/// Recursively converts `value` into an equivalent [`serde_json::Value`].
///
/// A non-finite [`Value::Number`] (`NaN`/`+-Infinity` — unreachable from the
/// synthetic `github.event` payload this module exists for, since that
/// payload only ever carries strings and small integers, but a defensive
/// case here rather than a silent `serde_json` panic) has no JSON
/// representation, so it becomes `null` rather than inventing a value.
pub(crate) fn to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Array(items) => {
            serde_json::Value::Array(items.items().iter().map(to_json).collect())
        }
        Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.to_string(), to_json(value)))
                .collect(),
        ),
    }
}
