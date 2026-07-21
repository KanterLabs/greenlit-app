//! `contains(search, item)` as documented at
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#contains>.

use crate::value::{Value, abstract_equal, ordinal_ignore_case_contains, to_display_string};

/// Never errors. Object search returns `false` (keys and values are not
/// searched), matching the runner's
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Contains.cs>.
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
