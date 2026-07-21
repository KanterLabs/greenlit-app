//! `startsWith(s, prefix)` / `endsWith(s, suffix)` as documented at
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#startswith>.

use crate::value::{
    Value, ordinal_ignore_case_ends_with, ordinal_ignore_case_starts_with, to_display_string,
};

/// Both arguments must be primitive, else `false`. Never errors.
pub(crate) fn starts_with(s: &Value, prefix: &Value) -> Value {
    if is_non_primitive(s) || is_non_primitive(prefix) {
        return Value::Bool(false);
    }
    Value::Bool(ordinal_ignore_case_starts_with(
        &to_display_string(s),
        &to_display_string(prefix),
    ))
}

/// Both arguments must be primitive, else `false`. Never errors.
pub(crate) fn ends_with(s: &Value, suffix: &Value) -> Value {
    if is_non_primitive(s) || is_non_primitive(suffix) {
        return Value::Bool(false);
    }
    Value::Bool(ordinal_ignore_case_ends_with(
        &to_display_string(s),
        &to_display_string(suffix),
    ))
}

fn is_non_primitive(v: &Value) -> bool {
    matches!(v, Value::Array(_) | Value::Object(_))
}
