//! The six-kind runtime value model and GitHub's exact coercion rules.
//!
//! Source for the kind set and every coercion below: the design memo's
//! "Type model and coercions" section (`Sdk/EvaluationResult.cs`,
//! `Sdk/ExpressionUtility.cs`), cross-referenced against the docs'
//! [Expressions reference](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions).

use std::cmp::Ordering;
use std::rc::Rc;

/// A GitHub Actions expression value. There are exactly six kinds
/// (`ValueKind` in the design memo, all numeric host types canonicalize to
/// [`Value::Number`] as `f64`).
///
/// `Array`/`Object` hold an `Rc` so that `==`/`!=` can implement GitHub's
/// documented "same instance" reference-identity rule for collections (see
/// [`abstract_equal`]) — consequently `Value` is `Clone` but not `Send`;
/// evaluation is expected to happen on a single thread per expression, which
/// matches how `greenlit-engine` calls into this crate (there is no
/// concurrent mutation of a value once built).
///
/// The derived [`PartialEq`] is ordinary Rust structural equality (deep
/// value comparison, including comparing `Array`/`Object` contents rather
/// than instance identity) — a convenience for assertions in this crate's
/// own tests and for callers who want a plain equality check. It is a
/// *different* operation from GitHub's own loose `==` semantics, which are
/// always the explicit [`abstract_equal`] function (never Rust's `==`
/// operator), because GitHub's coercion/identity rules would be a
/// surprising override of `PartialEq`'s normal meaning.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// The `null` value.
    Null,
    /// A `true`/`false` boolean.
    Bool(bool),
    /// A number — GitHub Actions has exactly one numeric kind, an IEEE-754
    /// `f64` (no separate integer type).
    Number(f64),
    /// A string.
    String(String),
    /// An array (see [`ArrayValue`] for the plain-vs-filtered distinction).
    Array(ArrayValue),
    /// An object (insertion-ordered, case-insensitive key lookup).
    Object(ObjectValue),
}

/// The kind tag of a [`Value`], used throughout the coercion rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// [`Value::Null`]'s kind.
    Null,
    /// [`Value::Bool`]'s kind.
    Boolean,
    /// [`Value::Number`]'s kind.
    Number,
    /// [`Value::String`]'s kind.
    String,
    /// [`Value::Array`]'s kind.
    Array,
    /// [`Value::Object`]'s kind.
    Object,
}

impl Value {
    /// Returns this value's [`ValueKind`].
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Null => ValueKind::Null,
            Value::Bool(_) => ValueKind::Boolean,
            Value::Number(_) => ValueKind::Number,
            Value::String(_) => ValueKind::String,
            Value::Array(_) => ValueKind::Array,
            Value::Object(_) => ValueKind::Object,
        }
    }

    /// Builds an ordinary (non-filtered) array value from owned items.
    pub fn array(items: Vec<Value>) -> Value {
        Value::Array(ArrayValue::new(items, false))
    }

    /// Builds a filtered-array value (the result of an object-filter `*`
    /// expression) from owned items. See [`ArrayValue::is_filtered`] for why
    /// this is a distinct flavor.
    pub fn filtered_array(items: Vec<Value>) -> Value {
        Value::Array(ArrayValue::new(items, true))
    }

    /// Builds an object value from owned, insertion-ordered entries. Keys
    /// are looked up case-insensitively later (see [`ObjectValue::get`]);
    /// duplicate keys are the caller's responsibility to avoid (matching
    /// GitHub's `DictionaryContextData`, which is itself a single map — the
    /// last write for a given key wins if a caller inserts a duplicate).
    pub fn object(entries: Vec<(String, Value)>) -> Value {
        Value::Object(ObjectValue::new(entries))
    }
}

/// An array value. See the design memo's "Index / dereference evaluation"
/// section for why filtered arrays are a distinct internal flavor: indexing
/// a *filtered* array by string key maps the lookup over each element
/// (silently skipping elements that don't have it), whereas indexing an
/// *ordinary* array by string key converts the key with `ToNumber` (almost
/// always `NaN` for a real property name) and returns `Null`. Both flavors
/// are otherwise ordinary arrays for the rest of the type system (`join`,
/// `contains`, `toJSON`, truthiness).
#[derive(Debug, Clone, PartialEq)]
pub struct ArrayValue {
    items: Rc<Vec<Value>>,
    filtered: bool,
}

impl ArrayValue {
    fn new(items: Vec<Value>, filtered: bool) -> Self {
        ArrayValue {
            items: Rc::new(items),
            filtered,
        }
    }

    /// The array's elements, in order.
    pub fn items(&self) -> &[Value] {
        &self.items
    }

    /// The number of elements.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the array has no elements.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether this array is the result of an object-filter `*` expression.
    pub fn is_filtered(&self) -> bool {
        self.filtered
    }

    /// Reference-identity check for `==`/`!=` (see [`abstract_equal`]):
    /// "Objects and arrays are only considered equal when they are the same
    /// instance."
    fn same_instance(&self, other: &ArrayValue) -> bool {
        Rc::ptr_eq(&self.items, &other.items)
    }
}

/// An object value: insertion-ordered key/value pairs, looked up
/// case-insensitively.
///
/// Source: the design memo's "Index / dereference evaluation" section —
/// "the runner's `DictionaryContextData` uses `StringComparer.OrdinalIgnoreCase`;
/// the dictionary preserves insertion order." A linear scan is used for
/// lookup rather than a second case-folded index: context objects in
/// practice (env maps, `github.event` sub-objects, matrix entries) are small,
/// and a linear scan avoids maintaining two representations of the same
/// key that could drift.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectValue {
    entries: Rc<Vec<(String, Value)>>,
}

impl ObjectValue {
    fn new(entries: Vec<(String, Value)>) -> Self {
        ObjectValue {
            entries: Rc::new(entries),
        }
    }

    /// Case-insensitive ordinal key lookup (see the module doc comment on
    /// [`ordinal_ignore_case_eq`] for exactly what "ordinal" means here).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(k, _)| ordinal_ignore_case_eq(k, key))
            .map(|(_, v)| v)
    }

    /// Iterates entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the object has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn same_instance(&self, other: &ObjectValue) -> bool {
        Rc::ptr_eq(&self.entries, &other.entries)
    }
}

// ---------------------------------------------------------------------
// ToBoolean (truthiness)
// ---------------------------------------------------------------------

/// GitHub's truthiness rule (`EvaluationResult.IsFalsy`).
///
/// Docs list falsy as `false, 0, -0, "", '', null`; the design memo notes
/// the *source* additionally makes `NaN` falsy, which the docs omit — the
/// source wins per this crate's fidelity policy (match documented behavior,
/// then observed/source behavior). Every other value (including `"0"`,
/// `"false"`, an empty array, an empty object) is truthy.
pub fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !*b,
        Value::Number(n) => *n == 0.0 || n.is_nan(),
        Value::String(s) => s.is_empty(),
        Value::Array(_) | Value::Object(_) => false,
    }
}

/// `!is_falsy`, for readability at call sites.
pub fn is_truthy(v: &Value) -> bool {
    !is_falsy(v)
}

// ---------------------------------------------------------------------
// ToNumber
// ---------------------------------------------------------------------

/// GitHub's `ToNumber` coercion (`EvaluationResult.ConvertToNumber` +
/// `ExpressionUtility.ParseNumber`, described as "follows JavaScript
/// `Number()`" by the design memo).
pub fn to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => *n,
        Value::String(s) => string_to_number(s),
        // "Array -> NaN (empty array too)"; "Object -> NaN (empty object too)".
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

/// String→number per the memo: trim Unicode whitespace; empty → 0; else a
/// JSON-ish decimal parse (lenient forms `.5`/`1.`/`+3` allowed, no interior
/// whitespace or thousands separators); else lowercase `0x`/`0o` (i32
/// range); else the case-insensitive `"Infinity"`/`"-Infinity"` symbols;
/// else `NaN`.
///
/// `ExpressionUtility.ParseNumber` delegates to `Double.TryParse` first.
/// .NET documents its `NaN` and infinity symbol comparisons as
/// case-insensitive, while the runner's explicit radix checks use lowercase
/// `x`/`o` exactly.
/// <https://learn.microsoft.com/en-us/dotnet/api/system.double.tryparse>
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/ExpressionUtility.cs>
fn string_to_number(s: &str) -> f64 {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    if trimmed.eq_ignore_ascii_case("Infinity") {
        return f64::INFINITY;
    }
    if trimmed
        .strip_prefix('+')
        .is_some_and(|symbol| symbol.eq_ignore_ascii_case("Infinity"))
    {
        return f64::INFINITY;
    }
    if trimmed.eq_ignore_ascii_case("-Infinity") {
        return f64::NEG_INFINITY;
    }
    if let Some(hex) = trimmed.strip_prefix("0x") {
        if !hex.is_empty()
            && hex.chars().all(|c| c.is_ascii_hexdigit())
            && let Ok(n) = i32::from_str_radix(hex, 16)
        {
            return f64::from(n);
        }
        return f64::NAN;
    }
    if let Some(oct) = trimmed.strip_prefix("0o") {
        if !oct.is_empty()
            && oct.chars().all(|c| ('0'..='7').contains(&c))
            && let Ok(n) = i32::from_str_radix(oct, 8)
        {
            return f64::from(n);
        }
        return f64::NAN;
    }
    // Reject forms Rust's f64 parser is more lenient about than JS
    // Number()/GitHub (bare "inf"/"infinity"/"nan" case-insensitively, and
    // interior whitespace is already excluded by construction since we
    // split on nothing — `trim` only removes the ends).
    let digits_part = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if digits_part.is_empty()
        || !digits_part
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-')
        || !digits_part.chars().any(|c| c.is_ascii_digit())
    {
        return f64::NAN;
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

// ---------------------------------------------------------------------
// ToString
// ---------------------------------------------------------------------

/// GitHub's `ToString` coercion (`EvaluationResult.ConvertToString`) — also
/// the interpolation stringifier used by `${{ }}` inside YAML strings
/// (template-layer, `greenlit-workflow`'s concern, but the per-value
/// stringification rule itself lives here since it's shared).
pub fn to_display_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => format_g15(*n),
        Value::String(s) => s.clone(),
        // "the literal string 'Array'" / "'Object'".
        Value::Array(_) => "Array".to_string(),
        Value::Object(_) => "Object".to_string(),
    }
}

/// Formats a number the way .NET's `"G15"` invariant format string does:
/// round to 15 significant digits, fixed-point when the decimal exponent is
/// strictly greater than `-5` and less than `15`,
/// otherwise scientific with uppercase `E`, an explicit sign,
/// and at least two exponent digits; trailing zeros stripped; `-0`
/// preserved; `NaN`/`±Infinity` as their literal names.
///
/// Source: design memo §2.3 ("G15 semantics to replicate exactly"). Exact
/// behavior at extreme exponents is flagged there as needing a
/// fuzz-compare against real GitHub output; the boundary cases named in the
/// memo (`1E+20`, `1E-06`, `-0`) are pinned through the crate's end-to-end
/// oracle table.
pub fn format_g15(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if n == 0.0 {
        return if n.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }

    let negative = n.is_sign_negative();
    let abs = n.abs();

    // 15 significant digits via scientific notation with 14 fractional
    // digits (1 leading + 14 = 15 total); Rust's formatter performs correct
    // round-half-to-even rounding and exponent-carry adjustment for us.
    // `{:e}` always produces `<mantissa>e<exponent>` with a parseable
    // integer exponent for a finite, non-NaN `f64` (both already ruled out
    // above), so the fallbacks below are unreachable in practice — they
    // exist only so this function has no panicking path at all, per
    // `AGENTS.md`'s "no `unwrap`/`expect`/`panic!`" quality bar.
    let sci = format!("{abs:.14e}");
    let Some((mantissa_str, exp_str)) = sci.split_once('e') else {
        return "0".to_string();
    };
    let exp: i32 = exp_str.parse().unwrap_or(0);
    let digits: String = mantissa_str.chars().filter(|c| *c != '.').collect();
    let trimmed = digits.trim_end_matches('0');
    let trimmed = if trimmed.is_empty() { "0" } else { trimmed };

    // .NET's general-format rule is strictly `exponent > -5`, not `>=`.
    // https://learn.microsoft.com/en-us/dotnet/standard/base-types/standard-numeric-format-strings
    if exp > -5 && exp < 15 {
        fixed_point(trimmed, exp, negative)
    } else {
        scientific(trimmed, exp, negative)
    }
}

/// Renders `trimmed` significant digits (an implied `d.ddd… * 10^exp`) as a
/// fixed-point decimal string.
fn fixed_point(trimmed: &str, exp: i32, negative: bool) -> String {
    let sign = if negative { "-" } else { "" };
    if exp >= 0 {
        let int_len = (exp as usize) + 1;
        if trimmed.len() <= int_len {
            let mut s = trimmed.to_string();
            s.push_str(&"0".repeat(int_len - trimmed.len()));
            format!("{sign}{s}")
        } else {
            let (int_part, frac_part) = trimmed.split_at(int_len);
            format!("{sign}{int_part}.{frac_part}")
        }
    } else {
        let zeros = "0".repeat((-exp - 1) as usize);
        format!("{sign}0.{zeros}{trimmed}")
    }
}

/// Renders `trimmed` significant digits as `[-]d[.ddd]E±NN` (minimum two
/// exponent digits).
fn scientific(trimmed: &str, exp: i32, negative: bool) -> String {
    let sign = if negative { "-" } else { "" };
    let mantissa = if trimmed.len() == 1 {
        trimmed.to_string()
    } else {
        format!("{}.{}", &trimmed[0..1], &trimmed[1..])
    };
    let exp_sign = if exp >= 0 { "+" } else { "-" };
    format!("{sign}{mantissa}E{exp_sign}{:02}", exp.abs())
}

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
