//! Newtonsoft-compatible numeric token scanning and conversion.

use super::token::{ParsedNumber, ParsedValue};
use super::{FromJsonError, JsonParser, is_keyword_separator, is_newtonsoft_whitespace};

impl JsonParser {
    pub(super) fn parse_number_or_signed_keyword(
        &mut self,
        in_constructor: bool,
    ) -> Result<ParsedValue, FromJsonError> {
        if self.chars[self.pos..].starts_with(&['-', 'I', 'n', 'f', 'i', 'n', 'i', 't', 'y'])
            && is_keyword_separator(&self.chars, self.pos + "-Infinity".len(), in_constructor)
        {
            self.pos += "-Infinity".len();
            return Ok(ParsedValue::Number(ParsedNumber {
                value: f64::NEG_INFINITY,
                rendering: "-Infinity".to_owned(),
            }));
        }

        let start = self.pos;
        while let Some(c) = self.peek() {
            if is_newtonsoft_number_char(c) {
                self.pos += 1;
            } else if is_number_separator(c) {
                break;
            } else {
                return Err(self.err(format!("unexpected character {c:?} while parsing a number")));
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        self.parse_scanned_number(&text).map(ParsedValue::Number)
    }

    fn parse_scanned_number(&self, text: &str) -> Result<ParsedNumber, FromJsonError> {
        let bytes = text.as_bytes();
        let non_base_ten = bytes.first() == Some(&b'0')
            && bytes.len() > 1
            && !matches!(bytes[1], b'.' | b'e' | b'E');
        if non_base_ten {
            let (digits, radix) = if text
                .get(..2)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("0x"))
            {
                (&text[2..], 16)
            } else {
                (text, 8)
            };
            if digits.is_empty() {
                return Err(self.err(format!("invalid number literal {text:?}")));
            }
            // Convert.ToInt64's base-8/base-16 overload accepts the full
            // unsigned 64-bit bit pattern and interprets its top bit as the
            // sign bit, hence the deliberate u64 -> i64 cast.
            return u64::from_str_radix(digits, radix)
                .map(|value| value as i64)
                .map(|value| ParsedNumber {
                    value: value as f64,
                    rendering: value.to_string(),
                })
                .map_err(|_| self.err(format!("invalid number literal {text:?}")));
        }

        // Newtonsoft parses integral tokens as Int64, then BigInteger on
        // overflow, but refuses JavaScript integer spellings longer than 380
        // characters. `ToPipelineContextData` finally converts either to
        // Double.
        // https://github.com/JamesNK/Newtonsoft.Json/blob/13.0.3/Src/Newtonsoft.Json/JsonTextReader.cs#L2149-L2227
        let digits = text.strip_prefix('-').unwrap_or(text);
        let is_decimal_integer =
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit());
        if is_decimal_integer {
            let int64 = if digits.len() <= 19 {
                text.parse::<i64>().ok()
            } else {
                None
            };
            if let Some(value) = int64 {
                return Ok(ParsedNumber {
                    value: value as f64,
                    rendering: value.to_string(),
                });
            }
            if text.len() > 380 {
                return Err(self.err(format!("number literal {text:?} is too large to parse")));
            }
            return text
                .parse::<f64>()
                .map(|value| ParsedNumber {
                    // `BigInteger` has no signed zero; its explicit
                    // conversion to Double therefore yields positive zero
                    // even when the authored token starts with `-`.
                    value: if value == 0.0 { 0.0 } else { value },
                    rendering: normalize_big_integer(text),
                })
                .map_err(|_| self.err(format!("invalid number literal {text:?}")));
        }
        text.parse::<f64>()
            .map(|value| ParsedNumber {
                value,
                rendering: format_dotnet_roundtrip(value),
            })
            .map_err(|_| self.err(format!("invalid number literal {text:?}")))
    }
}

fn normalize_big_integer(text: &str) -> String {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, text),
    };
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return "0".to_owned();
    }
    if negative {
        format!("-{digits}")
    } else {
        digits.to_owned()
    }
}

/// .NET 8's `Double.ToString("R", InvariantCulture)` uses the shortest
/// round-trippable digits, fixed notation for exponents -4 through 16, and a
/// signed, at-least-two-digit exponent otherwise. JsonConvert then appends
/// `.0` when a finite float has neither decimal point nor exponent.
fn format_dotnet_roundtrip(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == f64::INFINITY {
        return "Infinity".to_owned();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_owned();
    }

    let shortest = value.to_string();
    let magnitude = value.abs();
    let mut rendered = if magnitude != 0.0 && !(1e-4..1e17).contains(&magnitude) {
        scientific_notation(&shortest)
    } else {
        shortest
    };
    if !rendered.contains(['.', 'e', 'E']) {
        rendered.push_str(".0");
    }
    rendered
}

fn scientific_notation(shortest: &str) -> String {
    let (negative, unsigned) = match shortest.strip_prefix('-') {
        Some(unsigned) => (true, unsigned),
        None => (false, shortest),
    };
    if let Some((mantissa, exponent)) = unsigned.split_once(['e', 'E']) {
        let exponent = exponent.parse::<i32>().unwrap_or(0);
        return format_exponent(negative, mantissa, exponent);
    }

    let decimal = unsigned.find('.').unwrap_or(unsigned.len());
    let digits: String = unsigned.chars().filter(|c| *c != '.').collect();
    let first_nonzero = digits.find(|c| c != '0').unwrap_or(0);
    let exponent = decimal as i32 - first_nonzero as i32 - 1;
    let significant = digits[first_nonzero..].trim_end_matches('0');
    let mut mantissa = String::new();
    mantissa.push(significant.chars().next().unwrap_or('0'));
    if significant.len() > 1 {
        mantissa.push('.');
        mantissa.push_str(&significant[1..]);
    }
    format_exponent(negative, &mantissa, exponent)
}

fn format_exponent(negative: bool, mantissa: &str, exponent: i32) -> String {
    let sign = if negative { "-" } else { "" };
    let exponent_sign = if exponent < 0 { '-' } else { '+' };
    format!(
        "{sign}{mantissa}E{exponent_sign}{:02}",
        exponent.unsigned_abs()
    )
}

fn is_newtonsoft_number_char(c: char) -> bool {
    matches!(
        c,
        '-' | '+'
            | 'a'..='f'
            | 'A'..='F'
            | 'x'
            | 'X'
            | '.'
            | '0'..='9'
    )
}

fn is_number_separator(c: char) -> bool {
    is_newtonsoft_whitespace(c) || matches!(c, ',' | '}' | ']' | ')' | '/')
}
