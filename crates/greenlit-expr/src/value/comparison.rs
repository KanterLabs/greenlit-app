//! Ordinal string comparison and loose equality/ordering.

use std::cmp::Ordering;

use super::{Value, to_number};

// ---------------------------------------------------------------------
// Ordinal, case-insensitive string comparison
// ---------------------------------------------------------------------

/// GitHub's string equality/ordering is .NET `OrdinalIgnoreCase`: ordinal
/// (per-UTF-16-code-unit) comparison with ASCII case equivalence, not
/// Rust's full Unicode case mapping. In particular, Microsoft's contract
/// example keeps Turkish dotless `ı` distinct from ASCII `I`/`i`.
/// <https://learn.microsoft.com/en-us/dotnet/api/system.stringcomparer.ordinalignorecase>
pub fn ordinal_ignore_case_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Case-folds ASCII letters only. Shared by every
/// case-insensitive string comparison in this crate (`==`, `<`/`>`,
/// `contains`/`startsWith`/`endsWith`'s substring tests).
fn fold_ascii_upper(s: &str) -> String {
    s.to_ascii_uppercase()
}

/// Case-insensitive substring test, used by `contains()` (design memo §3.1).
pub fn ordinal_ignore_case_contains(haystack: &str, needle: &str) -> bool {
    fold_ascii_upper(haystack).contains(&fold_ascii_upper(needle))
}

/// Case-insensitive prefix test, used by `startsWith()` (design memo §3.2).
pub fn ordinal_ignore_case_starts_with(s: &str, prefix: &str) -> bool {
    fold_ascii_upper(s).starts_with(&fold_ascii_upper(prefix))
}

/// Case-insensitive suffix test, used by `endsWith()` (design memo §3.2).
pub fn ordinal_ignore_case_ends_with(s: &str, suffix: &str) -> bool {
    fold_ascii_upper(s).ends_with(&fold_ascii_upper(suffix))
}

/// Ordinal (UTF-16 code-unit) comparison after case folding. Comparing by
/// UTF-16 code unit rather than by Rust's native `char`/UTF-8 byte order
/// matters at the boundary between the Basic Multilingual Plane private-use
/// area (`U+E000..=U+FFFF`) and astral-plane characters (`U+10000` and
/// above): UTF-16 encodes astral characters as a surrogate pair in the
/// `0xD800..=0xDFFF` range, which sorts *before* `0xE000..=0xFFFF`, whereas
/// comparing by Unicode scalar value (what `str`'s own `Ord` does, matching
/// UTF-8 byte order) sorts astral characters *after* all BMP characters.
/// See design memo §2.6 and backlog item 4.
pub fn ordinal_ignore_case_cmp(a: &str, b: &str) -> Ordering {
    let to_utf16 = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
    to_utf16(&fold_ascii_upper(a)).cmp(&to_utf16(&fold_ascii_upper(b)))
}

// ---------------------------------------------------------------------
// Loose ("abstract") equality and relational comparison
// ---------------------------------------------------------------------

/// `CoerceTypes` (design memo §2.5): repeatedly narrows a pair of values
/// toward a common kind before comparing. Terminates because a `Bool`/`Null`
/// operand always converts to `Number` (never back), and a `Number`↔`String`
/// pair converts the `String` side to `Number` and then has equal kinds.
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

/// GitHub's loose equality (`==`), a.k.a. `AbstractEqual` — "similar to JS
/// abstract equality, except string comparison is `OrdinalIgnoreCase`, and
/// objects are not coerced to primitives." Design memo §2.5; never errors.
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

/// `a != b`, defined as exactly `!(a == b)` (design memo §1.2).
pub fn abstract_not_equal(a: &Value, b: &Value) -> bool {
    !abstract_equal(a, b)
}

/// `a < b` per GitHub's relational rules (design memo §2.6): coercion as in
/// [`abstract_equal`]; differing kinds after coercion, `NaN` operands, and
/// `Array`/`Object` operands are all `false`.
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

/// `a <= b`, defined as exactly `(a == b) || (a < b)`, "including re-running
/// coercions" (design memo §2.6) — i.e. genuinely calling both underlying
/// functions rather than sharing one coercion pass.
pub fn less_or_equal(a: &Value, b: &Value) -> bool {
    abstract_equal(a, b) || less_than(a, b)
}

/// `a >= b`, defined as exactly `(a == b) || (a > b)`.
pub fn greater_or_equal(a: &Value, b: &Value) -> bool {
    abstract_equal(a, b) || greater_than(a, b)
}
