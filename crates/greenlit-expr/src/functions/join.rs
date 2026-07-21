//! `join(array, sep?)` — design memo §3.4.
//!
//! The "separator is only evaluated if the array has ≥2 elements" laziness
//! rule is enforced by the caller (`eval::evaluate`'s `Call` dispatch, which
//! decides *whether* to evaluate the second argument expression at all
//! before calling [`join`]) — this function is the pure post-evaluation
//! logic, taking an already-resolved (or absent) separator value.

use crate::value::{Value, to_display_string};

/// Never errors.
pub(crate) fn join(array: &Value, sep: Option<&Value>) -> Value {
    match array {
        Value::Array(arr) => {
            if arr.is_empty() {
                return Value::String(String::new());
            }
            // "if a 2nd arg is present and primitive, its ToString (may be
            // empty); a non-primitive separator is silently ignored
            // (default `,` used)."
            let separator = sep
                .filter(|v| !matches!(v, Value::Array(_) | Value::Object(_)))
                .map(to_display_string)
                .unwrap_or_else(|| ",".to_string());
            Value::String(
                arr.items()
                    .iter()
                    .map(to_display_string)
                    .collect::<Vec<_>>()
                    .join(&separator),
            )
        }
        // "If arg0 is an Object ... empty string."
        Value::Object(_) => Value::String(String::new()),
        // "If arg0 is primitive (incl. null): result is just ToString(arg0)."
        primitive => Value::String(to_display_string(primitive)),
    }
}
