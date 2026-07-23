//! JsonTextReader's legacy JavaScript `new Name(...)` extension.

use super::token::ParsedValue;
use super::{FromJsonError, JsonParser, is_dotnet_letter_or_digit, is_keyword_separator};

impl JsonParser {
    pub(super) fn parse_constructor(&mut self, depth: usize) -> Result<ParsedValue, FromJsonError> {
        let keyword: Vec<char> = "new".chars().collect();
        if !self.chars[self.pos..].starts_with(&keyword)
            || !is_keyword_separator(&self.chars, self.pos + keyword.len(), false)
        {
            return Err(self.err("unexpected content while parsing JSON"));
        }
        self.pos += keyword.len();
        self.skip_whitespace();

        let name_start = self.pos;
        while self.peek().is_some_and(is_dotnet_letter_or_digit) {
            self.pos += 1;
        }
        let name: String = self.chars[name_start..self.pos].iter().collect();
        if name.is_empty() {
            return Err(self.err("constructor name cannot be empty"));
        }
        self.skip_whitespace();
        if self.bump() != Some('(') {
            return Err(self.err("expected '(' after constructor name"));
        }

        let mut arguments = Vec::new();
        self.skip_insignificant()?;
        if self.peek() == Some(')') {
            self.bump();
            return Ok(ParsedValue::Constructor { name, arguments });
        }
        loop {
            arguments.push(self.parse_value(depth, true)?);
            self.skip_insignificant()?;
            match self.peek() {
                Some(',') => {
                    self.bump();
                    self.skip_insignificant()?;
                    if self.peek() == Some(')') {
                        self.bump();
                        return Ok(ParsedValue::Constructor { name, arguments });
                    }
                }
                Some(')') => {
                    self.bump();
                    return Ok(ParsedValue::Constructor { name, arguments });
                }
                _ => return Err(self.err("expected ',' or ')' in constructor")),
            }
        }
    }
}
