//! Lenient `fromJSON` parsing compatible with the Actions runner.

use crate::value::Value;
use unicode_general_category::{GeneralCategory, get_general_category};

mod constructor;
mod number;
mod string;
mod token;

use token::ParsedValue;

/// `JsonReader.MaxDepth` defaults to 64, and `FromJson.cs` does not override
/// it on the `JsonTextReader` it constructs.
/// https://github.com/JamesNK/Newtonsoft.Json/blob/13.0.3/Src/Newtonsoft.Json/JsonReader.cs
const MAX_JSON_DEPTH: usize = 64;

/// A `fromJSON()` failure — always a parse failure (the argument is first
/// `ToString`-converted, which never fails; parsing the resulting text can).
#[derive(Debug, thiserror::Error)]
#[error("fromJSON() could not parse the input as JSON at character {position}: {message}")]
pub struct FromJsonError {
    position: usize,
    message: String,
}

/// `fromJSON(value)`. The runner parses through Newtonsoft's `JsonTextReader`
/// and `JToken.ReadFrom`:
/// <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/FromJson.cs>.
/// This gives
/// Newtonsoft-compatible *leniency*: single-quoted strings, unquoted object
/// keys, `//`/`/* */` comments, trailing commas, and bare `NaN`/`Infinity`
/// literals are all accepted (unlike strict `serde_json`). Trailing content
/// after the first complete value is ignored, matching `JToken.ReadFrom`'s
/// behavior of not checking for additional content.
pub(crate) fn from_json(text: &str) -> Result<Value, FromJsonError> {
    let mut p = JsonParser {
        chars: text.chars().collect(),
        pos: 0,
    };
    p.skip_whitespace();
    // `JToken.ReadFrom` does not pass `JsonLoadSettings`, so an initial
    // comment is itself the root JToken. `ToPipelineContextData` converts that
    // otherwise-unknown token to its comment text. Comments encountered by a
    // container loader are ignored instead (see `skip_insignificant`).
    // https://github.com/JamesNK/Newtonsoft.Json/blob/13.0.3/Src/Newtonsoft.Json/Linq/JToken.cs#L2119-L2178
    if p.peek() == Some('/') {
        return p.parse_comment().map(Value::String);
    }
    let value = p.parse_value(0, false)?;
    Ok(value.into_expression_value())
}

struct JsonParser {
    chars: Vec<char>,
    pos: usize,
}

impl JsonParser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn err(&self, message: impl Into<String>) -> FromJsonError {
        FromJsonError {
            position: self.pos,
            message: message.into(),
        }
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(is_newtonsoft_whitespace) {
            self.pos += 1;
        }
    }

    /// Skips whitespace and comments while loading a container. Newtonsoft's
    /// default `JsonLoadSettings.CommentHandling` is `Ignore` for arrays,
    /// objects, and constructors.
    fn skip_insignificant(&mut self) -> Result<(), FromJsonError> {
        loop {
            match self.peek() {
                Some(c) if is_newtonsoft_whitespace(c) => {
                    self.pos += 1;
                }
                Some('/') => {
                    self.parse_comment()?;
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn parse_comment(&mut self) -> Result<String, FromJsonError> {
        let start = self.pos;
        if self.bump() != Some('/') {
            return Err(self.err("expected a comment"));
        }
        match self.bump() {
            Some('/') => {
                let content_start = self.pos;
                while !matches!(self.peek(), None | Some('\r') | Some('\n')) {
                    self.pos += 1;
                }
                if self.peek().is_none() && self.pos == content_start {
                    return Err(self.err("unexpected end while parsing comment"));
                }
                Ok(self.chars[content_start..self.pos].iter().collect())
            }
            Some('*') => {
                let content_start = self.pos;
                while let Some(c) = self.peek() {
                    if c == '*' && self.peek_at(1) == Some('/') {
                        let comment = self.chars[content_start..self.pos].iter().collect();
                        self.pos += 2;
                        return Ok(comment);
                    }
                    self.pos += 1;
                }
                self.pos = start;
                Err(self.err("unexpected end while parsing comment"))
            }
            Some(other) => Err(self.err(format!(
                "expected '*' or '/' after comment opener, got {other:?}"
            ))),
            None => Err(self.err("unexpected end while parsing comment")),
        }
    }

    fn parse_value(
        &mut self,
        enclosing_depth: usize,
        in_constructor: bool,
    ) -> Result<ParsedValue, FromJsonError> {
        self.skip_insignificant()?;
        match self.peek() {
            None => Err(self.err("unexpected end of input")),
            Some('{') => self.parse_object(self.enter_container(enclosing_depth)?),
            Some('[') => self.parse_array(self.enter_container(enclosing_depth)?),
            Some('"') | Some('\'') => Ok(ParsedValue::String(self.parse_quoted_string()?)),
            Some(',') => Ok(ParsedValue::Undefined),
            Some(c) if c == '-' || c.is_ascii_digit() || c == '.' => {
                self.parse_number_or_signed_keyword(in_constructor)
            }
            Some('n') if self.peek_at(1) == Some('e') => {
                self.parse_constructor(self.enter_container(enclosing_depth)?)
            }
            Some(c) if c.is_ascii_alphabetic() => self.parse_bare_keyword(in_constructor),
            Some(c) => Err(self.err(format!("unexpected character {c:?}"))),
        }
    }

    fn enter_container(&self, enclosing_depth: usize) -> Result<usize, FromJsonError> {
        if enclosing_depth >= MAX_JSON_DEPTH {
            Err(self.err(format!(
                "the JSON reader's maximum depth of {MAX_JSON_DEPTH} has been exceeded"
            )))
        } else {
            Ok(enclosing_depth + 1)
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<ParsedValue, FromJsonError> {
        self.bump(); // '{'
        let mut entries: Vec<(String, ParsedValue)> = Vec::new();
        self.skip_insignificant()?;
        if self.peek() == Some('}') {
            self.bump();
            return Ok(ParsedValue::object(entries));
        }
        loop {
            self.skip_insignificant()?;
            let key = match self.peek() {
                Some('"') | Some('\'') => self.parse_quoted_string()?,
                Some(c) if is_newtonsoft_identifier_char(c) => self.parse_unquoted_key(),
                _ => return Err(self.err("expected an object key")),
            };
            // JsonTextReader.ParseProperty calls EatWhitespace here, not the
            // comment parser. A comment between a key and `:` is invalid.
            self.skip_whitespace();
            if self.bump() != Some(':') {
                return Err(self.err("expected ':' after object key"));
            }
            let value = self.parse_value(depth, false)?;
            // Preserve every occurrence here and let `Value::object`
            // perform the runner-compatible last-value-wins normalization
            // in one indexed pass. Deduplicating this growing vector by
            // linear search made a large authored object quadratic twice.
            entries.push((key, value));
            self.skip_insignificant()?;
            match self.peek() {
                Some(',') => {
                    self.bump();
                    self.skip_insignificant()?;
                    if self.peek() == Some('}') {
                        // Trailing comma allowed.
                        self.bump();
                        return Ok(ParsedValue::object(entries));
                    }
                }
                Some('}') => {
                    self.bump();
                    return Ok(ParsedValue::object(entries));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<ParsedValue, FromJsonError> {
        self.bump(); // '['
        let mut items = Vec::new();
        self.skip_insignificant()?;
        if self.peek() == Some(']') {
            self.bump();
            return Ok(ParsedValue::Array(items));
        }
        loop {
            let value = self.parse_value(depth, false)?;
            items.push(value);
            self.skip_insignificant()?;
            match self.peek() {
                Some(',') => {
                    self.bump();
                    self.skip_insignificant()?;
                    if self.peek() == Some(']') {
                        // Trailing comma allowed.
                        self.bump();
                        return Ok(ParsedValue::Array(items));
                    }
                }
                Some(']') => {
                    self.bump();
                    return Ok(ParsedValue::Array(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn parse_unquoted_key(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if is_newtonsoft_identifier_char(c) {
                self.pos += 1;
            } else {
                break;
            }
        }
        self.chars[start..self.pos].iter().collect()
    }

    fn parse_bare_keyword(&mut self, in_constructor: bool) -> Result<ParsedValue, FromJsonError> {
        for (literal, value) in [
            ("null", ParsedValue::Null),
            ("true", ParsedValue::Bool(true)),
            ("false", ParsedValue::Bool(false)),
            (
                "NaN",
                ParsedValue::Number(token::ParsedNumber {
                    value: f64::NAN,
                    rendering: "NaN".to_owned(),
                }),
            ),
            (
                "Infinity",
                ParsedValue::Number(token::ParsedNumber {
                    value: f64::INFINITY,
                    rendering: "Infinity".to_owned(),
                }),
            ),
            ("undefined", ParsedValue::Undefined),
        ] {
            if self.consume_keyword(literal, in_constructor) {
                return Ok(value);
            }
        }
        Err(self.err("unrecognized token"))
    }

    fn consume_keyword(&mut self, literal: &str, in_constructor: bool) -> bool {
        let literal: Vec<char> = literal.chars().collect();
        if !self.chars[self.pos..].starts_with(&literal) {
            return false;
        }
        let end = self.pos + literal.len();
        if !is_keyword_separator(&self.chars, end, in_constructor) {
            return false;
        }
        self.pos = end;
        true
    }
}

fn is_newtonsoft_identifier_char(c: char) -> bool {
    if c == '_' || c == '$' {
        return true;
    }
    is_dotnet_letter_or_digit(c)
}

fn is_dotnet_letter_or_digit(c: char) -> bool {
    // JsonTextReader iterates UTF-16 `char` code units. A non-BMP scalar is a
    // surrogate pair there, and neither surrogate is LetterOrDigit.
    if u32::from(c) > u32::from(u16::MAX) {
        return false;
    }
    matches!(
        get_general_category(c),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
            | GeneralCategory::DecimalNumber
    )
}

fn is_keyword_separator(chars: &[char], position: usize, in_constructor: bool) -> bool {
    match chars.get(position).copied() {
        None => true,
        Some(c) if is_newtonsoft_whitespace(c) || matches!(c, '}' | ']' | ',') => true,
        Some(')') if in_constructor => true,
        Some('/') => matches!(chars.get(position + 1), Some('*' | '/')),
        _ => false,
    }
}

fn is_newtonsoft_whitespace(c: char) -> bool {
    // JsonTextReader 13.0.3 delegates non-ASCII whitespace to
    // `char.IsWhiteSpace`. .NET 8 derives that predicate from Unicode's
    // White_Space property. Spell out its complete BMP set so a future Rust
    // Unicode table update cannot silently alter the accepted JSON grammar.
    // https://github.com/JamesNK/Newtonsoft.Json/blob/13.0.3/Src/Newtonsoft.Json/JsonTextReader.cs#L1835-L1868
    // https://github.com/dotnet/runtime/blob/v8.0.0/src/libraries/System.Private.CoreLib/src/System/Globalization/CharUnicodeInfo.cs#L947-L965
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{0085}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
    )
}
