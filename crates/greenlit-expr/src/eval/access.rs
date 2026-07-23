//! Property, index, and wildcard access during expression evaluation.

use crate::error::EvalError;
use crate::memory::{MIN_OBJECT_BYTES, MemoryCounter, POINTER_BYTES, wrapped_primitive_bytes};
use crate::value::{ArrayValue, Value, to_display_string, to_number};

/// `target[index]` / `target.property`, following the runner's
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Operators/Index.cs>.
pub(super) fn index_into(
    target: &Value,
    index: &Value,
    max_memory_bytes: usize,
) -> Result<(Option<Value>, Option<usize>, bool), EvalError> {
    let result = match target {
        // "Target is not a collection (null or primitive): result is Null."
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => (None, None, false),
        Value::Object(obj) => {
            // "if its result is a primitive it is converted with ToString
            // ... Missing key or non-primitive index -> Null."
            if matches!(index, Value::Array(_) | Value::Object(_)) {
                return Ok((None, None, false));
            }
            let key = to_display_string(index);
            let value = obj.get(&key).cloned();
            let bytes = match value.as_ref() {
                Some(value) => wrapped_primitive_bytes(value, max_memory_bytes)?,
                None => None,
            };
            let has_raw = bytes.is_some();
            (value, bytes, has_raw)
        }
        Value::Array(arr) if arr.is_filtered() => {
            let (value, bytes) = index_into_filtered(arr, index, max_memory_bytes)?;
            (Some(value), Some(bytes), false)
        }
        Value::Array(arr) => {
            let value = index_into_plain_array(arr, index);
            let bytes = match value.as_ref() {
                Some(value) => wrapped_primitive_bytes(value, max_memory_bytes)?,
                None => None,
            };
            let has_raw = bytes.is_some();
            (value, bytes, has_raw)
        }
    };
    Ok(result)
}

/// "Target is an array: index result -> ToNumber; if NaN or `< 0` -> Null;
/// `floor()` it; if `> i32::MAX` -> Null; if `>= len` -> Null; else the
/// element."
fn index_into_plain_array(arr: &ArrayValue, index: &Value) -> Option<Value> {
    let n = to_number(index);
    if n.is_nan() || n < 0.0 {
        return None;
    }
    let floored = n.floor();
    if floored > f64::from(i32::MAX) {
        return None;
    }
    arr.items().get(floored as usize).cloned()
}

/// Indexing a *filtered* array follows `Index.cs`: for each item that is
/// an object and has the key (case-insensitive), append its value; items
/// lacking the key are silently skipped; non-object items skipped" for a
/// string-shaped index, and "for each item that is an array with that
/// index in range, append that element" for an integer-shaped one.
/// Per-element dispatch (object items tried as a key lookup, array items
/// tried as a numeric lookup) rather than picking one mode for the whole
/// call reproduces this correctly even when a filtered array holds a mix
/// of object and array elements, using the single evaluated index value's
/// two coercions (`ToString` for object items, `ToNumber` for array items).
fn index_into_filtered(
    arr: &ArrayValue,
    index: &Value,
    max_memory_bytes: usize,
) -> Result<(Value, usize), EvalError> {
    if matches!(index, Value::Array(_) | Value::Object(_)) {
        // Not documented explicitly for filtered arrays; consistent with
        // every other "non-primitive index" case in this function, which
        // is always Null/empty rather than an error.
        return Ok((Value::filtered_array(vec![]), 0));
    }
    let as_key = to_display_string(index);
    let as_number = to_number(index);
    let mut out = Vec::new();
    let mut counter = MemoryCounter::new(max_memory_bytes);
    for item in arr.items() {
        match item {
            Value::Object(o) => {
                if let Some(v) = o.get(&as_key) {
                    counter.add(POINTER_BYTES)?;
                    out.push(v.clone());
                }
            }
            Value::Array(a) => {
                if as_number.is_nan() || as_number < 0.0 {
                    continue;
                }
                let floored = as_number.floor();
                if floored > f64::from(i32::MAX) {
                    continue;
                }
                if let Some(v) = a.items().get(floored as usize) {
                    counter.add(POINTER_BYTES)?;
                    out.push(v.clone());
                }
            }
            _ => {}
        }
    }
    Ok((Value::filtered_array(out), counter.current_bytes()))
}

/// `target.*` / `target[*]`, the object-filter syntax documented at
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#object-filters>.
pub(super) fn wildcard_filter(
    target: &Value,
    max_memory_bytes: usize,
) -> Result<(Value, Option<usize>), EvalError> {
    let result = match target {
        // "Filtered arrays ... `.*`: flatten one level — for each item
        // that is an object append all its values, for each array item
        // append all elements, scalar items skipped."
        Value::Array(arr) if arr.is_filtered() => {
            let mut out = Vec::new();
            let mut counter = MemoryCounter::new(max_memory_bytes);
            for item in arr.items() {
                match item {
                    Value::Object(o) => {
                        for (_, value) in o.iter() {
                            counter.add(POINTER_BYTES)?;
                            out.push(value.clone());
                        }
                    }
                    Value::Array(a) => {
                        for value in a.items() {
                            counter.add(POINTER_BYTES)?;
                            out.push(value.clone());
                        }
                    }
                    _ => {}
                }
            }
            (Value::filtered_array(out), Some(counter.current_bytes()))
        }
        // "Wildcard `*` on an array: filtered array of all elements in
        // order."
        Value::Array(arr) => {
            let mut counter = MemoryCounter::new(max_memory_bytes);
            counter.add(MIN_OBJECT_BYTES)?;
            let mut out = Vec::new();
            for value in arr.items() {
                counter.add(POINTER_BYTES)?;
                out.push(value.clone());
            }
            (Value::filtered_array(out), Some(counter.current_bytes()))
        }
        // "On an object: filtered array of all values in the object's
        // stored order."
        Value::Object(obj) => {
            let mut counter = MemoryCounter::new(max_memory_bytes);
            counter.add(MIN_OBJECT_BYTES)?;
            let mut out = Vec::new();
            for (_, value) in obj.iter() {
                counter.add(POINTER_BYTES)?;
                out.push(value.clone());
            }
            (Value::filtered_array(out), Some(counter.current_bytes()))
        }
        // "unless the index is the wildcard `*`, in which case the result
        // is an empty filtered array" (the null-or-primitive-target case).
        _ => (Value::filtered_array(vec![]), None),
    };
    Ok(result)
}
