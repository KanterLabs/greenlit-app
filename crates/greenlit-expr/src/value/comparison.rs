//! Ordinal string comparison and loose equality/ordering.

use std::cmp::Ordering;

use super::{Value, to_number};

// ---------------------------------------------------------------------
// Ordinal, case-insensitive string comparison
// ---------------------------------------------------------------------

/// GitHub's string equality/ordering delegates to .NET
/// `StringComparison.OrdinalIgnoreCase`.
///
/// The runner calls it directly for equality and relational comparison:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs>.
/// .NET defines the comparison as invariant uppercase followed by ordinal
/// comparison, including non-ASCII casing:
/// <https://learn.microsoft.com/en-us/dotnet/api/system.stringcomparer.ordinalignorecase>.
pub fn ordinal_ignore_case_eq(a: &str, b: &str) -> bool {
    ordinal_uppercase(a) == ordinal_uppercase(b)
}

/// Applies the one-scalar uppercase mapping used by .NET's ordinal comparer.
///
/// Rust's [`char::to_uppercase`] exposes full Unicode casing and can expand a
/// scalar (for example `ß` to `SS`). .NET's ordinal implementation instead
/// maps one Unicode scalar to one scalar while preserving UTF-16 width, so an
/// expansion must remain unchanged. .NET also deliberately keeps dotless `ı`
/// unchanged for invariant casing. These details are visible in the runtime's
/// casing implementation:
/// <https://github.com/dotnet/runtime/blob/main/src/libraries/System.Private.CoreLib/src/System/Globalization/InvariantModeCasing.cs>
/// and
/// <https://github.com/dotnet/runtime/blob/main/src/native/libs/System.Globalization.Native/pal_casing.c>.
fn ordinal_upper(character: char) -> char {
    if character == '\u{0131}' {
        return character;
    }

    let mut uppercase = character.to_uppercase();
    match (uppercase.next(), uppercase.next()) {
        (Some(mapped), None) if mapped.len_utf16() == character.len_utf16() => mapped,
        _ => character,
    }
}

/// Produces the invariant, one-scalar uppercase representation shared by all
/// OrdinalIgnoreCase operations in this crate.
fn ordinal_uppercase(s: &str) -> String {
    s.chars().map(ordinal_upper).collect()
}

/// Case-insensitive substring test, matching the runner's
/// `IndexOf(..., StringComparison.OrdinalIgnoreCase)` implementation:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Contains.cs>.
pub fn ordinal_ignore_case_contains(haystack: &str, needle: &str) -> bool {
    ordinal_uppercase(haystack).contains(&ordinal_uppercase(needle))
}

/// Case-insensitive prefix test, matching the runner's
/// `StringComparison.OrdinalIgnoreCase` implementation:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/StartsWith.cs>.
pub fn ordinal_ignore_case_starts_with(s: &str, prefix: &str) -> bool {
    ordinal_uppercase(s).starts_with(&ordinal_uppercase(prefix))
}

/// Case-insensitive suffix test, matching the runner's
/// `StringComparison.OrdinalIgnoreCase` implementation:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/EndsWith.cs>.
pub fn ordinal_ignore_case_ends_with(s: &str, suffix: &str) -> bool {
    ordinal_uppercase(s).ends_with(&ordinal_uppercase(suffix))
}

/// Ordinal (UTF-16 code-unit) comparison after invariant uppercase mapping.
/// Comparing by
/// UTF-16 code unit rather than by Rust's native `char`/UTF-8 byte order
/// matters at the boundary between the Basic Multilingual Plane private-use
/// area (`U+E000..=U+FFFF`) and astral-plane characters (`U+10000` and
/// above): UTF-16 encodes astral characters as a surrogate pair in the
/// `0xD800..=0xDFFF` range, which sorts *before* `0xE000..=0xFFFF`, whereas
/// comparing by Unicode scalar value (what `str`'s own `Ord` does, matching
/// UTF-8 byte order) sorts astral characters *after* all BMP characters.
/// This matches .NET's runtime implementation:
/// <https://github.com/dotnet/runtime/blob/main/src/libraries/System.Private.CoreLib/src/System/Globalization/Ordinal.cs>.
pub fn ordinal_ignore_case_cmp(a: &str, b: &str) -> Ordering {
    let to_utf16 = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
    to_utf16(&ordinal_uppercase(a)).cmp(&to_utf16(&ordinal_uppercase(b)))
}

// ---------------------------------------------------------------------
// Loose ("abstract") equality and relational comparison
// ---------------------------------------------------------------------

/// The runner's `CoerceTypes` repeatedly narrows a pair of values toward a
/// common kind before comparing:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs>.
/// This terminates because a `Bool`/`Null` operand always converts to `Number`
/// (never back), and a `Number`↔`String` pair converts the `String` side to
/// `Number` and then has equal kinds.
fn coerce_for_compare(mut a: Value, mut b: Value) -> (Value, Value) {
    loop {
        if a.kind() == b.kind() {
            return (a, b);
        }
        match (&a, &b) {
            (Value::Number(_), Value::String(_)) => {
                b = Value::Number(to_number(&b));
                continue;
            }
            (Value::String(_), Value::Number(_)) => {
                a = Value::Number(to_number(&a));
                continue;
            }
            _ => {}
        }
        let mut changed = false;
        if matches!(a, Value::Bool(_) | Value::Null) {
            a = Value::Number(to_number(&a));
            changed = true;
        }
        if matches!(b, Value::Bool(_) | Value::Null) {
            b = Value::Number(to_number(&b));
            changed = true;
        }
        if !changed {
            // Object/Array vs. a different kind: coercion cannot proceed,
            // kinds stay different.
            return (a, b);
        }
    }
}

/// GitHub's loose equality (`==`), a.k.a. `AbstractEqual`: similar to JS
/// abstract equality, except string comparison is `OrdinalIgnoreCase` and
/// objects are not coerced to primitives. The runner implementation is the
/// primary source:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs>.
pub fn abstract_equal(a: &Value, b: &Value) -> bool {
    let (a, b) = coerce_for_compare(a.clone(), b.clone());
    if a.kind() != b.kind() {
        return false;
    }
    match (&a, &b) {
        (Value::Null, Value::Null) => true,
        (Value::Number(x), Value::Number(y)) => {
            if x.is_nan() || y.is_nan() {
                false
            } else {
                x == y
            }
        }
        (Value::String(x), Value::String(y)) => ordinal_ignore_case_eq(x, y),
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => x.same_instance(y),
        (Value::Object(x), Value::Object(y)) => x.same_instance(y),
        // The `a.kind() != b.kind()` check above guarantees this is never
        // reached; `false` (not a panic) is the safe fallback, per
        // `AGENTS.md`'s "no `unwrap`/`expect`/`panic!`" quality bar.
        _ => false,
    }
}

/// `a != b`, defined by the runner as exactly `!(a == b)`:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs>.
pub fn abstract_not_equal(a: &Value, b: &Value) -> bool {
    !abstract_equal(a, b)
}

/// `a < b` per the runner's relational rules: coercion as in
/// [`abstract_equal`]; differing kinds after coercion, `NaN` operands, and
/// `Array`/`Object` operands are all `false`:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs>.
pub fn less_than(a: &Value, b: &Value) -> bool {
    let (a, b) = coerce_for_compare(a.clone(), b.clone());
    if a.kind() != b.kind() {
        return false;
    }
    match (&a, &b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_nan() || y.is_nan() {
                false
            } else {
                x < y
            }
        }
        (Value::String(x), Value::String(y)) => ordinal_ignore_case_cmp(x, y) == Ordering::Less,
        // "Boolean: true > false (GT <=> l && !r; LT <=> !l && r)".
        (Value::Bool(x), Value::Bool(y)) => !x && *y,
        _ => false,
    }
}

/// `a > b`, the mirror of [`less_than`].
pub fn greater_than(a: &Value, b: &Value) -> bool {
    let (a, b) = coerce_for_compare(a.clone(), b.clone());
    if a.kind() != b.kind() {
        return false;
    }
    match (&a, &b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_nan() || y.is_nan() {
                false
            } else {
                x > y
            }
        }
        (Value::String(x), Value::String(y)) => ordinal_ignore_case_cmp(x, y) == Ordering::Greater,
        (Value::Bool(x), Value::Bool(y)) => *x && !y,
        _ => false,
    }
}

/// `a <= b`, defined by the runner as exactly `(a == b) || (a < b)`, including
/// re-running coercions through both underlying functions:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs>.
pub fn less_or_equal(a: &Value, b: &Value) -> bool {
    abstract_equal(a, b) || less_than(a, b)
}

/// `a >= b`, defined as exactly `(a == b) || (a > b)`.
pub fn greater_or_equal(a: &Value, b: &Value) -> bool {
    abstract_equal(a, b) || greater_than(a, b)
}
