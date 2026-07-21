//! Lenient `fromJSON` parsing compatible with the Actions runner.

use crate::value::Value;

/// A `fromJSON()` failure — always a parse failure (the argument is first
/// `ToString`-converted, which never fails; parsing the resulting text can).
#[derive(Debug, thiserror::Error)]
#[error("fromJSON() could not parse the input as JSON at character {position}: {message}")]
pub struct FromJsonError {
    position: usize,
    message: String,
}

/// `fromJSON(value)`. Source: design memo §3.6 — parsed with
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
    p.skip_insignificant();
    let value = p.parse_value()?;
    Ok(value)
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

    /// Skips whitespace, `//` line comments, and `/* */` block comments —
    /// Newtonsoft's lenient-reader extensions the design memo calls out.
    fn skip_insignificant(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.pos += 1;
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    self.pos += 2;
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    self.pos += 2;
                    while self.peek().is_some()
                        && !(self.peek() == Some('*') && self.peek_at(1) == Some('/'))
                    {
                        self.pos += 1;
                    }
                    self.pos += 2;
                }
                _ => break,
            }
        }
    }

    fn parse_value(&mut self) -> Result<Value, FromJsonError> {
        self.skip_insignificant();
        match self.peek() {
            None => Err(self.err("unexpected end of input")),
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') | Some('\'') => Ok(Value::String(self.parse_quoted_string()?)),
            Some(c) if c == '-' || c == '+' || c.is_ascii_digit() || c == '.' => {
                self.parse_number_or_signed_keyword()
            }
            Some(c) if c.is_ascii_alphabetic() => self.parse_bare_keyword(),
            Some(c) => Err(self.err(format!("unexpected character {c:?}"))),
        }
    }

    fn parse_object(&mut self) -> Result<Value, FromJsonError> {
        self.bump(); // '{'
        let mut entries: Vec<(String, Value)> = Vec::new();
        self.skip_insignificant();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(Value::object(entries));
        }
        loop {
            self.skip_insignificant();
            let key = match self.peek() {
                Some('"') | Some('\'') => self.parse_quoted_string()?,
                Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                    self.parse_unquoted_key()
                }
                _ => return Err(self.err("expected an object key")),
            };
            self.skip_insignificant();
            if self.bump() != Some(':') {
                return Err(self.err("expected ':' after object key"));
            }
            let value = self.parse_value()?;
            if let Some(existing) = entries.iter_mut().find(|(k, _)| *k == key) {
                existing.1 = value;
            } else {
                entries.push((key, value));
            }
            self.skip_insignificant();
            match self.peek() {
                Some(',') => {
                    self.bump();
                    self.skip_insignificant();
                    if self.peek() == Some('}') {
                        // Trailing comma allowed.
                        self.bump();
                        return Ok(Value::object(entries));
                    }
                }
                Some('}') => {
                    self.bump();
                    return Ok(Value::object(entries));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn parse_array(&mut self) -> Result<Value, FromJsonError> {
        self.bump(); // '['
        let mut items = Vec::new();
        self.skip_insignificant();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(Value::array(items));
        }
        loop {
            let value = self.parse_value()?;
            items.push(value);
            self.skip_insignificant();
            match self.peek() {
                Some(',') => {
                    self.bump();
                    self.skip_insignificant();
                    if self.peek() == Some(']') {
                        // Trailing comma allowed.
                        self.bump();
                        return Ok(Value::array(items));
                    }
                }
                Some(']') => {
                    self.bump();
                    return Ok(Value::array(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn parse_unquoted_key(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                self.pos += 1;
            } else {
                break;
            }
        }
        self.chars[start..self.pos].iter().collect()
    }

    fn parse_quoted_string(&mut self) -> Result<String, FromJsonError> {
        let quote = self.bump().ok_or_else(|| self.err("expected a string"))?;
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some(c) if c == quote => break,
                Some('\\') => match self.bump() {
                    Some('"') => s.push('"'),
                    Some('\'') => s.push('\''),
                    Some('\\') => s.push('\\'),
                    Some('/') => s.push('/'),
                    Some('b') => s.push('\u{8}'),
                    Some('f') => s.push('\u{c}'),
                    Some('n') => s.push('\n'),
                    Some('r') => s.push('\r'),
                    Some('t') => s.push('\t'),
                    Some('u') => {
                        let high = self.parse_hex_u16()?;
                        let scalar = if (0xd800..=0xdbff).contains(&high) {
                            if self.bump() != Some('\\') || self.bump() != Some('u') {
                                return Err(
                                    self.err("high surrogate must be followed by a low surrogate")
                                );
                            }
                            let low = self.parse_hex_u16()?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return Err(
                                    self.err("high surrogate must be followed by a low surrogate")
                                );
                            }
                            0x1_0000
                                + ((u32::from(high) - 0xd800) << 10)
                                + (u32::from(low) - 0xdc00)
                        } else if (0xdc00..=0xdfff).contains(&high) {
                            return Err(self.err("low surrogate has no preceding high surrogate"));
                        } else {
                            u32::from(high)
                        };
                        // JSON `\uXXXX` represents UTF-16 code units. A
                        // surrogate pair is one scalar, just as Newtonsoft's
                        // JsonTextReader produces in the runner.
                        // https://github.com/JamesNK/Newtonsoft.Json/blob/master/Src/Newtonsoft.Json/JsonTextReader.cs
                        match char::from_u32(scalar) {
                            Some(c) => s.push(c),
                            None => return Err(self.err("invalid \\u escape codepoint")),
                        }
                    }
                    _ => return Err(self.err("invalid escape sequence")),
                },
                Some(c) => s.push(c),
            }
        }
        Ok(s)
    }

    fn parse_hex_u16(&mut self) -> Result<u16, FromJsonError> {
        let mut code = 0u16;
        for _ in 0..4 {
            let digit = self.bump().and_then(|c| c.to_digit(16));
            match digit {
                Some(digit) => code = code * 16 + digit as u16,
                None => return Err(self.err("invalid \\u escape")),
            }
        }
        Ok(code)
    }

    fn parse_bare_keyword(&mut self) -> Result<Value, FromJsonError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        match text.as_str() {
            "null" => Ok(Value::Null),
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "NaN" => Ok(Value::Number(f64::NAN)),
            "Infinity" => Ok(Value::Number(f64::INFINITY)),
            other => Err(self.err(format!("unrecognized token {other:?}"))),
        }
    }

    fn parse_number_or_signed_keyword(&mut self) -> Result<Value, FromJsonError> {
        if self.peek() == Some('-')
            && self.chars[self.pos + 1..].starts_with(&['I', 'n', 'f', 'i', 'n', 'i', 't', 'y'])
        {
            self.pos += 1 + "Infinity".len();
            return Ok(Value::Number(f64::NEG_INFINITY));
        }
        let start = self.pos;
        if matches!(self.peek(), Some('-') | Some('+')) {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some('.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            self.pos += 1;
            if matches!(self.peek(), Some('-') | Some('+')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| self.err(format!("invalid number literal {text:?}")))
    }
}
