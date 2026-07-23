//! `join(array, sep?)` as documented at
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#join>.
//!
//! The "separator is only evaluated if the array has ≥2 elements" laziness
//! and ordering rule is enforced here through a callback: the first item is
//! converted and memory-checked before the separator expression runs.

use crate::error::EvalError;
use crate::memory::MemoryCounter;
use crate::value::{Value, to_display_string};

/// Joins values while enforcing the runner's node-local result budget.
pub(crate) fn join(
    array: &Value,
    separator_argument_present: bool,
    max_memory_bytes: usize,
    eval_separator: impl FnOnce() -> Result<Value, EvalError>,
) -> Result<Value, EvalError> {
    match array {
        Value::Array(arr) => {
            if arr.is_empty() {
                return Ok(Value::String(String::new()));
            }
            // "if a 2nd arg is present and primitive, its ToString (may be
            // empty); a non-primitive separator is silently ignored
            // (default `,` used)."
            let mut result = String::new();
            let mut memory = MemoryCounter::new(max_memory_bytes);
            let first = to_display_string(&arr.items()[0]);
            memory.add_string(&first)?;
            result.push_str(&first);

            if arr.len() > 1 {
                let separator = if separator_argument_present {
                    let value = eval_separator()?;
                    if matches!(value, Value::Array(_) | Value::Object(_)) {
                        ",".to_string()
                    } else {
                        to_display_string(&value)
                    }
                } else {
                    ",".to_string()
                };
                for item in &arr.items()[1..] {
                    // `Join.cs` counts each separator as a separate .NET
                    // string, including an empty separator.
                    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Join.cs#L22-L60
                    memory.add_string(&separator)?;
                    result.push_str(&separator);
                    let item = to_display_string(item);
                    memory.add_string(&item)?;
                    result.push_str(&item);
                }
            }
            Ok(Value::String(result))
        }
        // "If arg0 is an Object ... empty string."
        Value::Object(_) => Ok(Value::String(String::new())),
        // "If arg0 is primitive (incl. null): result is just ToString(arg0)."
        primitive => Ok(Value::String(to_display_string(primitive))),
    }
}
