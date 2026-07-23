//! `format(fmt, arg0, …, argN)` — the runner accepts one to 255 total
//! arguments, so a format string with no placeholders is valid and at most
//! 254 replacement values may be supplied.
//! <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionConstants.cs>
//!
//! Value arguments are evaluated lazily (only indices actually referenced by
//! a placeholder are evaluated at all) and cached (a repeated `{0}` in the
//! format string evaluates `arg0` only once) — see [`format`]'s `eval_arg`
//! callback parameter, driven by `eval::evaluate`'s `Call` dispatch for
//! `format` specifically.

use crate::error::EvalError;
use crate::memory::MemoryCounter;
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
    /// A placeholder had a non-empty `:spec` section. The runner rejects
    /// non-empty format specifiers for every value kind; see
    /// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Format.cs>.
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

/// One token yielded by [`FormatScanner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatToken {
    /// One literal output character, after `{{`/`}}` unescaping.
    Literal(char),
    /// One `{N}` or `{N:spec}` placeholder.
    Placeholder {
        /// Zero-based value-argument index.
        index: usize,
        /// The optional format specifier, empty when none was authored.
        spec: String,
    },
}

/// Incremental scanner for GitHub's `format()` pattern language.
///
/// It is public so the planner can stop exactly when a referenced argument
/// becomes runtime-deferred, preserving the runner's evaluation/error order
/// without duplicating the pattern grammar.
#[derive(Debug, Clone)]
pub struct FormatScanner {
    chars: Vec<char>,
    at: usize,
    value_arg_count: usize,
}

impl FormatScanner {
    /// Starts scanning `fmt`, validating placeholder indices against
    /// `value_arg_count` as they are reached.
    #[must_use]
    pub fn new(fmt: &str, value_arg_count: usize) -> Self {
        Self {
            chars: fmt.chars().collect(),
            at: 0,
            value_arg_count,
        }
    }

    /// Returns the next literal or placeholder in runtime scan order.
    pub fn next_token(&mut self) -> Result<Option<FormatToken>, FormatError> {
        if self.at >= self.chars.len() {
            return Ok(None);
        }
        match self.chars[self.at] {
            '{' if peek(&self.chars, self.at + 1) == Some('{') => {
                self.at += 2;
                Ok(Some(FormatToken::Literal('{')))
            }
            '{' => self.scan_placeholder().map(Some),
            '}' if peek(&self.chars, self.at + 1) == Some('}') => {
                self.at += 2;
                Ok(Some(FormatToken::Literal('}')))
            }
            '}' => Err(FormatError::InvalidFormatString),
            character => {
                self.at += 1;
                Ok(Some(FormatToken::Literal(character)))
            }
        }
    }

    fn scan_placeholder(&mut self) -> Result<FormatToken, FormatError> {
        let digits_start = self.at + 1;
        let mut cursor = digits_start;
        while cursor < self.chars.len() && self.chars[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == digits_start {
            return Err(FormatError::InvalidFormatString);
        }
        let digits: String = self.chars[digits_start..cursor].iter().collect();
        let index = digits
            .parse::<usize>()
            .map_err(|_| FormatError::InvalidFormatString)?;

        let mut spec = String::new();
        if self.chars.get(cursor) == Some(&':') {
            cursor += 1;
            loop {
                if cursor >= self.chars.len() {
                    return Err(FormatError::InvalidFormatString);
                }
                if self.chars[cursor] == '}' {
                    if peek(&self.chars, cursor + 1) == Some('}') {
                        spec.push('}');
                        cursor += 2;
                        continue;
                    }
                    break;
                }
                spec.push(self.chars[cursor]);
                cursor += 1;
            }
        }
        if self.chars.get(cursor) != Some(&'}') {
            return Err(FormatError::InvalidFormatString);
        }
        self.at = cursor + 1;
        if index >= self.value_arg_count {
            return Err(FormatError::TooManyArgumentsReferenced {
                index,
                supplied: self.value_arg_count,
            });
        }
        Ok(FormatToken::Placeholder { index, spec })
    }
}

/// Scans `fmt` and renders it, calling `eval_arg(index)` at most once per
/// distinct referenced index (`0`-based, into the value-argument list of
/// length `value_arg_count`) and caching the result.
pub(crate) fn format(
    fmt: &str,
    value_arg_count: usize,
    max_memory_bytes: usize,
    mut eval_arg: impl FnMut(usize) -> Result<Value, EvalError>,
) -> Result<Value, EvalError> {
    let mut cache: Vec<Option<Value>> = vec![None; value_arg_count];
    let mut scanner = FormatScanner::new(fmt, value_arg_count);
    let mut out = String::new();
    let mut literal_segment = String::new();
    let mut counter = MemoryCounter::new(max_memory_bytes);
    while let Some(part) = scanner.next_token()? {
        match part {
            FormatToken::Literal(character) => {
                literal_segment.push(character);
                // The runner appends each substring ending in an escaped
                // brace as one segment. Each segment carries the 26-byte
                // .NET string overhead in MemoryCounter's accounting.
                // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Format.cs#L214-L225
                if character == '{' || character == '}' {
                    append_segment(&mut out, &mut literal_segment, &mut counter)?;
                }
            }
            FormatToken::Placeholder { index, spec } => {
                append_segment(&mut out, &mut literal_segment, &mut counter)?;
                if !spec.is_empty() {
                    let value = get_or_eval(&mut cache, index, &mut eval_arg)?;
                    return Err(FormatError::UnsupportedFormatSpecifier {
                        spec,
                        kind: value.kind(),
                    }
                    .into());
                }

                let value = get_or_eval(&mut cache, index, &mut eval_arg)?;
                let rendered = to_display_string(&value);
                if !rendered.is_empty() {
                    counter.add_string(&rendered)?;
                    out.push_str(&rendered);
                }
            }
        }
    }
    append_segment(&mut out, &mut literal_segment, &mut counter)?;

    Ok(Value::String(out))
}

fn append_segment(
    out: &mut String,
    segment: &mut String,
    counter: &mut MemoryCounter,
) -> Result<(), EvalError> {
    if !segment.is_empty() {
        counter.add_string(segment)?;
        out.push_str(segment);
        segment.clear();
    }
    Ok(())
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
