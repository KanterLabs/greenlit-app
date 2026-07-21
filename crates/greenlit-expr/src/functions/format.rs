//! `format(fmt, arg0, …, argN)` — GitHub's public contract requires at
//! least one replacement value and specifies no maximum number of values.
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#format>
//!
//! Value arguments are evaluated lazily (only indices actually referenced by
//! a placeholder are evaluated at all) and cached (a repeated `{0}` in the
//! format string evaluates `arg0` only once) — see [`format`]'s `eval_arg`
//! callback parameter, driven by `eval::evaluate`'s `Call` dispatch for
//! `format` specifically.

use crate::error::EvalError;
use crate::value::{Value, ValueKind, to_display_string};

/// A `format()` failure.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    /// A `{` that isn't `{{` and isn't a valid `{N[:spec]}` placeholder, a
    /// bare `}` not part of `}}`, or an index too large for this platform.
    #[error("invalid format string")]
    InvalidFormatString,
    /// A placeholder referenced an argument position beyond how many value
    /// arguments were actually supplied.
    #[error(
        "the format string references more arguments than were supplied: index {index} (only {supplied} value argument(s) given)"
    )]
    TooManyArgumentsReferenced {
        /// The referenced (0-based) placeholder index.
        index: usize,
        /// How many value arguments were actually supplied.
        supplied: usize,
    },
    /// A placeholder had a non-empty `:spec` section — any non-empty format
    /// specifier is unsupported for every `Value` kind (design memo: "any
    /// non-empty spec throws").
    #[error("the format specifiers {spec:?} are not valid for objects of type '{kind:?}'")]
    UnsupportedFormatSpecifier {
        /// The unsupported specifier text.
        spec: String,
        /// The referenced argument's value kind.
        kind: ValueKind,
    },
    /// Evaluating a referenced value argument's expression itself failed.
    #[error("{0}")]
    ArgumentEvaluation(Box<EvalError>),
}

fn peek(chars: &[char], at: usize) -> Option<char> {
    chars.get(at).copied()
}

/// Scans `fmt` and renders it, calling `eval_arg(index)` at most once per
/// distinct referenced index (`0`-based, into the value-argument list of
/// length `value_arg_count`) and caching the result.
pub(crate) fn format(
    fmt: &str,
    value_arg_count: usize,
    mut eval_arg: impl FnMut(usize) -> Result<Value, EvalError>,
) -> Result<Value, FormatError> {
    let mut cache: Vec<Option<Value>> = vec![None; value_arg_count];
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;

    while i < chars.len() {
        match chars[i] {
            '{' => {
                if peek(&chars, i + 1) == Some('{') {
                    out.push('{');
                    i += 2;
                    continue;
                }
                let digits_start = i + 1;
                let mut j = digits_start;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if j == digits_start {
                    return Err(FormatError::InvalidFormatString);
                }
                let digits: String = chars[digits_start..j].iter().collect();
                let index: usize = digits
                    .parse()
                    .map_err(|_| FormatError::InvalidFormatString)?;

                // Optional `{N:spec}` — `}}` inside the spec is a literal
                // `}`; the section ends at the first unescaped `}`.
                let mut spec = String::new();
                let mut k = j;
                if k < chars.len() && chars[k] == ':' {
                    k += 1;
                    loop {
                        if k >= chars.len() {
                            return Err(FormatError::InvalidFormatString);
                        }
                        if chars[k] == '}' {
                            if peek(&chars, k + 1) == Some('}') {
                                spec.push('}');
                                k += 2;
                                continue;
                            }
                            break;
                        }
                        spec.push(chars[k]);
                        k += 1;
                    }
                }
                if k >= chars.len() || chars[k] != '}' {
                    return Err(FormatError::InvalidFormatString);
                }
                i = k + 1;

                // "This check is done during the scan, so it fires even if
                // the expression would otherwise short-circuit."
                if index >= value_arg_count {
                    return Err(FormatError::TooManyArgumentsReferenced {
                        index,
                        supplied: value_arg_count,
                    });
                }

                if !spec.is_empty() {
                    let value = get_or_eval(&mut cache, index, &mut eval_arg)?;
                    return Err(FormatError::UnsupportedFormatSpecifier {
                        spec,
                        kind: value.kind(),
                    });
                }

                let value = get_or_eval(&mut cache, index, &mut eval_arg)?;
                out.push_str(&to_display_string(&value));
            }
            '}' => {
                if peek(&chars, i + 1) == Some('}') {
                    out.push('}');
                    i += 2;
                    continue;
                }
                return Err(FormatError::InvalidFormatString);
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }

    Ok(Value::String(out))
}

fn get_or_eval(
    cache: &mut [Option<Value>],
    index: usize,
    eval_arg: &mut impl FnMut(usize) -> Result<Value, EvalError>,
) -> Result<Value, FormatError> {
    if let Some(v) = &cache[index] {
        return Ok(v.clone());
    }
    let v = eval_arg(index).map_err(|e| FormatError::ArgumentEvaluation(Box::new(e)))?;
    cache[index] = Some(v.clone());
    Ok(v)
}
