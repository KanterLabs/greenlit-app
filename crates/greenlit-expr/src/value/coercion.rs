//! Truthiness, numeric conversion, and display-string conversion.

use super::Value;

// ---------------------------------------------------------------------
// ToBoolean (truthiness)
// ---------------------------------------------------------------------

/// GitHub's truthiness rule (`EvaluationResult.IsFalsy`).
///
/// GitHub's Expressions reference lists `false`, `0`, `-0`, `""`, `''`, and
/// `null` as falsy. The runner's `EvaluationResult.IsFalsy` additionally
/// treats `NaN` as falsy:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs>.
/// Every other value (including `"0"`, `"false"`, an empty array, and an
/// empty object) is truthy.
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

/// GitHub's `ToNumber` coercion. The public conversion table is documented
/// under
/// [expression operators](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#operators);
/// the runner implements it in `EvaluationResult.ConvertToNumber` and
/// `ExpressionUtility.ParseNumber`:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs>,
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/ExpressionUtility.cs>.
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

/// String-to-number conversion follows the runner's
/// `ExpressionUtility.ParseNumber`: trim Unicode whitespace; empty → 0; else a
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
/// `EvaluationResult.ConvertToString` formats numbers with the runner's
/// `ExpressionConstants.NumberFormat`, which is `G15`:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs>,
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionConstants.cs>.
/// The boundary cases `1E+20`, `1E-06`, and `-0` are pinned through the
/// crate's end-to-end oracle table.
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
