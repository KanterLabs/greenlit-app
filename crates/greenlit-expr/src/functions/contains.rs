//! `contains(search, item)` — design memo §3.1.

use crate::value::{Value, abstract_equal, ordinal_ignore_case_contains, to_display_string};

/// Never errors, per the design memo: "If `search` is an Object -> `false`
/// (keys/values are never searched)."
pub(crate) fn contains(search: &Value, item: &Value) -> Value {
    match search {
        // "If `search` is an Array (including filtered arrays): returns
        // true iff any element satisfies AbstractEqual(item, element)".
        Value::Array(arr) => Value::Bool(arr.items().iter().any(|el| abstract_equal(item, el))),
        // "If `search` is an Object -> false."
        Value::Object(_) => Value::Bool(false),
        // "If `search` is primitive: if `item` is also primitive, both ->
        // ToString, result = case-insensitive substring test; if `item` is
        // Object/Array -> false."
        primitive => match item {
            Value::Array(_) | Value::Object(_) => Value::Bool(false),
            _ => Value::Bool(ordinal_ignore_case_contains(
                &to_display_string(primitive),
                &to_display_string(item),
            )),
        },
    }
}
