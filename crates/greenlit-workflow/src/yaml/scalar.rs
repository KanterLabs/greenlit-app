//! GitHub's own YAML scalar-typing rules.
//!
//! GitHub does not resolve workflow YAML scalars using the plain YAML 1.2
//! core schema (which is what a general-purpose YAML library like `saphyr`
//! or `yaml-rust2` would give you by default, and which additionally accepts
//! `yes`/`no`/`on`/`off`/`y`/`n` as booleans under YAML 1.1 — the reason the
//! `on:` workflow key doesn't get misparsed). It instead runs each *plain*
//! scalar through the strict `MatchNull` / `MatchBoolean` / `MatchInteger` /
//! `MatchFloat` chain in the runner's `YamlObjectReader.cs` before falling
//! back to string:
//! <https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ObjectTemplating/YamlObjectReader.cs>.
//! Quoted and
//! block scalars (single/double-quoted, `|`, `>`) are never subject to this
//! matching — YAML's own quoting already pins them to string, and GitHub
//! honors that.
//!
//! This module is the sole place that implements those four matchers; every
//! caller goes through [`resolve`].

/// The four scalar kinds GitHub's core-schema-subset YAML typing produces,
/// plus the residual string case. This mirrors `greenlit-expr`'s expression
/// value kinds deliberately: numeric values use one `f64` representation,
/// so a value read from YAML and a value produced by expression evaluation
/// are shaped the same way once both crates meet in `greenlit-engine`.
#[derive(Debug, Clone, PartialEq)]
pub enum YamlScalar {
    /// Matched GitHub's Null grammar: `""`, `null`, `Null`, `NULL`, or `~`.
    Null,
    /// Matched GitHub's Boolean grammar (`true`/`True`/`TRUE`/`false`/
    /// `False`/`FALSE` only — critically, *not* `yes`/`no`/`on`/`off`).
    Bool(bool),
    /// Matched GitHub's Integer or Floating Point grammar. Always stored as
    /// `f64`, matching `greenlit-expr`'s single numeric kind.
    Number(f64),
    /// Did not match any of the above (or was quoted/block-style, which is
    /// never eligible), so the scalar's raw text is the value verbatim.
    String(String),
}

/// GitHub runner Null grammar: empty, `null`, `Null`, `NULL`,
/// or `~`, and nothing else (not YAML 1.1's `~`-adjacent spellings like
/// `Null`/`NULL` case variants beyond these exact four, nor bare `N`/`n`).
///
/// Exposed for explicit-tag handling (`!!null`): [`crate::yaml::raw`] uses
/// this directly after explicit-tag style validation.
pub(crate) fn as_null(raw: &str) -> bool {
    matches!(raw, "" | "null" | "Null" | "NULL" | "~")
}

/// GitHub runner Boolean grammar: exactly these six
/// spellings — `yes`/`no`/`on`/`off`/`y`/`n` are deliberately *not* matched
/// here (this is the documented reason `on:` works as a workflow key).
///
/// Exposed for explicit `!!bool` tag handling; see [`as_null`].
pub(crate) fn as_bool(raw: &str) -> Option<bool> {
    match raw {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

/// GitHub runner Integer grammar: `[0-9]+`, `[+-][0-9]+`,
/// `0x[0-9a-fA-F]+`, or `0o[0-7]+`. The sign belongs only to the decimal
/// branch; `-0x1` and `+0o7` therefore remain strings. Hex/octal literals
/// are parsed within `i32` range. Decimal overflow resolves to signed
/// infinity, matching `Double.TryParse` on the runner's .NET 8 target.
/// These branches transcribe `MatchInteger` in the current runner source:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ObjectTemplating/YamlObjectReader.cs>.
///
/// Exposed for explicit `!!int` tag handling; see [`as_null`].
pub(crate) fn as_integer(raw: &str) -> Option<Result<f64, NumberParseError>> {
    if let Some(hex_digits) = raw.strip_prefix("0x")
        && !hex_digits.is_empty()
        && hex_digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Some(parse_radix_within_i32(hex_digits, 16));
    }
    if let Some(oct_digits) = raw.strip_prefix("0o")
        && !oct_digits.is_empty()
        && oct_digits.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
    {
        return Some(parse_radix_within_i32(oct_digits, 8));
    }
    let decimal_digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    if !decimal_digits.is_empty() && decimal_digits.bytes().all(|b| b.is_ascii_digit()) {
        return Some(parse_decimal_number(raw));
    }
    None
}

/// A number-shaped scalar matched GitHub's grammar but failed conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumberParseError {
    /// An unsigned hexadecimal/octal magnitude did not fit `i32`.
    RadixIntegerOverflow,
    /// Rust's decimal parser rejected a grammar-valid scalar. The runner's
    /// `Double.TryParse` accepts overflow as infinity on .NET 8, so this is
    /// reserved for any residual parser mismatch rather than finite range.
    OutOfRange,
}

fn parse_radix_within_i32(digits: &str, radix: u32) -> Result<f64, NumberParseError> {
    i32::from_str_radix(digits, radix)
        .map(f64::from)
        .map_err(|_| NumberParseError::RadixIntegerOverflow)
}

/// GitHub runner Floating Point grammar:
/// `[+-]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][+-]?[0-9]+)?`, plus the special
/// spellings `.inf`/`.Inf`/`.INF` (optionally signed) and
/// `.nan`/`.NaN`/`.NAN`.
///
/// Once the grammar matches, the runner's `MatchFloat` uses
/// `Double.TryParse`. On its .NET 8 target, an out-of-range magnitude parses
/// successfully to signed infinity. Source: `MatchFloat` in
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ObjectTemplating/YamlObjectReader.cs>.
///
/// Exposed for explicit `!!float` tag handling; see [`as_null`].
pub(crate) fn as_float(raw: &str) -> Option<Result<f64, NumberParseError>> {
    match raw {
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            return Some(Ok(f64::INFINITY));
        }
        "-.inf" | "-.Inf" | "-.INF" => return Some(Ok(f64::NEG_INFINITY)),
        ".nan" | ".NaN" | ".NAN" => return Some(Ok(f64::NAN)),
        _ => {}
    }
    if matches_float_grammar(raw) {
        Some(parse_decimal_number(raw))
    } else {
        None
    }
}

fn parse_decimal_number(raw: &str) -> Result<f64, NumberParseError> {
    raw.parse::<f64>().map_err(|_| NumberParseError::OutOfRange)
}

/// Character-level conformance check for
/// `[+-]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][+-]?[0-9]+)?`, independent of
/// Rust's own (more permissive — it also accepts `inf`/`infinity`/`nan`
/// case-insensitively) `f64::from_str` grammar. Only strings that pass this
/// check are handed to `f64::from_str` for the actual value.
fn matches_float_grammar(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0usize;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let digits_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start {
            return false; // `.` with no digits after it.
        }
    } else {
        let digits_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start {
            return false; // No leading digits and no leading `.`.
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_digits_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_digits_start {
            return false; // `e`/`E` with no exponent digits.
        }
    }
    i == b.len()
}

/// Resolve a *plain-style* scalar's raw text against GitHub's Null →
/// Boolean → Integer → Floating Point → String matcher chain, in the order
/// implemented by `YamlObjectReader.cs`. The order matters:
/// `"123"` must resolve as an Integer, not fall through to Float, even
/// though the Float grammar's fractional part is optional and would also
/// accept it).
///
/// Not used for non-plain (quoted/block) scalars — callers must send those
/// straight to `YamlScalar::String` without calling this function, since
/// quoting already pins the value to string regardless of its content.
pub(crate) fn resolve_plain(raw: &str) -> Result<YamlScalar, NumberParseError> {
    if as_null(raw) {
        return Ok(YamlScalar::Null);
    }
    if let Some(b) = as_bool(raw) {
        return Ok(YamlScalar::Bool(b));
    }
    if let Some(int_result) = as_integer(raw) {
        return int_result.map(YamlScalar::Number);
    }
    if let Some(float_result) = as_float(raw) {
        return float_result.map(YamlScalar::Number);
    }
    Ok(YamlScalar::String(raw.to_owned()))
}

// Deliberately no `#[cfg(test)]` module here: this is a `pub(crate)`-only
// module unreachable from outside the crate, so any test of `resolve_plain`
// directly would necessarily be a colocated test of an internal helper —
// exactly what `TESTING.md`'s banned list rules out ("No tests for private
// functions or internal helpers. Internals are covered through the
// behaviors they serve."). The oracle table for this exact
// Null/Boolean/Integer/Float matcher instead lives in
// `tests/scalar_typing.rs`, driven through the crate's real public API
// (`parse_workflow`), so a refactor that preserves behavior here can't
// break a test that never should have known this function's name.
