//! Quoted-string parsing and Newtonsoft's UTF-16 replacement behavior.

use super::{FromJsonError, JsonParser};

impl JsonParser {
    pub(super) fn parse_quoted_string(&mut self) -> Result<String, FromJsonError> {
        let quote = self.bump().ok_or_else(|| self.err("expected a string"))?;
        let mut value = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some(c) if c == quote => break,
                Some('\\') => match self.bump() {
                    Some('"') => value.push('"'),
                    Some('\'') => value.push('\''),
                    Some('\\') => value.push('\\'),
                    Some('/') => value.push('/'),
                    Some('b') => value.push('\u{8}'),
                    Some('f') => value.push('\u{c}'),
                    Some('n') => value.push('\n'),
                    Some('r') => value.push('\r'),
                    Some('t') => value.push('\t'),
                    Some('u') => self.parse_unicode_escape(&mut value)?,
                    _ => return Err(self.err("invalid escape sequence")),
                },
                Some(c) => value.push(c),
            }
        }
        Ok(value)
    }

    /// JsonTextReader consumes `\uXXXX` as UTF-16 code units. Lone
    /// surrogates become U+FFFD; consecutive high surrogates each become
    /// U+FFFD until one is followed by a low surrogate.
    /// https://github.com/JamesNK/Newtonsoft.Json/blob/13.0.3/Src/Newtonsoft.Json/JsonTextReader.cs#L1201-L1254
    fn parse_unicode_escape(&mut self, out: &mut String) -> Result<(), FromJsonError> {
        let mut unit = self.parse_hex_u16()?;
        loop {
            if (0xdc00..=0xdfff).contains(&unit) {
                out.push(char::REPLACEMENT_CHARACTER);
                return Ok(());
            }
            if !(0xd800..=0xdbff).contains(&unit) {
                match char::from_u32(u32::from(unit)) {
                    Some(c) => out.push(c),
                    None => return Err(self.err("invalid \\u escape codepoint")),
                }
                return Ok(());
            }

            if self.peek() != Some('\\') || self.peek_at(1) != Some('u') {
                out.push(char::REPLACEMENT_CHARACTER);
                return Ok(());
            }
            self.pos += 2;
            let next = self.parse_hex_u16()?;
            if (0xdc00..=0xdfff).contains(&next) {
                let scalar =
                    0x1_0000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(next) - 0xdc00);
                match char::from_u32(scalar) {
                    Some(c) => out.push(c),
                    None => return Err(self.err("invalid UTF-16 surrogate pair")),
                }
                return Ok(());
            }

            out.push(char::REPLACEMENT_CHARACTER);
            unit = next;
        }
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
}
