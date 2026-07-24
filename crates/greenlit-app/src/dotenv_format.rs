//! Pure dotenv-syntax scanning, shared by `.litci/vars` (`crate::vars`) and
//! `.litci/secrets` (`crate::secrets`).
//!
//! This module knows nothing about paths, file I/O, or error-message
//! wording — each caller owns its own no-follow open/size-limit/error-text
//! policy (`crate::vars::dotenv`, `crate::secrets::dotenv`) and reduces to
//! this shared scanner only for the actual `KEY=VALUE` syntax: quoting,
//! escaping, comments, line continuation inside a quoted value, and the
//! `export` prefix. Keeping the two file-specific wrappers independent (each
//! with its own file-not-found/size/name-validation messages) while sharing
//! only this syntax core avoids re-deriving the escaping state machine twice
//! (`AGENTS.md` "Modules stay single-purpose") without coupling `.litci/vars`
//! and `.litci/secrets`' user-facing error text together.

/// One parsed `KEY=VALUE` assignment's line number is reported via
/// [`DotenvError`] rather than embedded in a formatted string, so each
/// caller renders its own path-qualified message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DotenvError {
    /// A physical line (or a quoted value's continuation) did not parse as
    /// `KEY=VALUE`, or a quoted value was never closed before end of file.
    Syntax {
        /// The first physical line of the malformed logical record.
        line: usize,
    },
    /// A syntactically valid `KEY` failed the caller-supplied name
    /// validator.
    InvalidName {
        /// The first physical line of the record.
        line: usize,
        /// The rejected name, exactly as authored (not canonicalized).
        name: String,
        /// Why the validator rejected it.
        reason: &'static str,
    },
    /// The file contained more assignments than `max_assignments` permits.
    AssignmentLimit,
}

enum ParsedLine {
    Skip,
    Entry(String, String),
    Incomplete,
}

#[derive(Clone, Copy)]
enum Quote {
    None,
    Single,
    Double,
}

struct ValueScan {
    quote: Quote,
    escaped: bool,
    expecting_end: bool,
    at_start: bool,
}

impl ValueScan {
    fn new() -> Self {
        Self {
            quote: Quote::None,
            escaped: false,
            expecting_end: false,
            at_start: true,
        }
    }

    fn completes_record(&mut self, input: &str) -> bool {
        for character in input.chars() {
            match self.quote {
                Quote::Single => {
                    if character == '\'' {
                        self.quote = Quote::None;
                    }
                }
                Quote::Double if self.escaped => self.escaped = false,
                Quote::Double => match character {
                    '"' => self.quote = Quote::None,
                    '\\' => self.escaped = true,
                    _ => {}
                },
                Quote::None if self.escaped => {
                    self.escaped = false;
                    self.at_start = false;
                }
                Quote::None if self.expecting_end => match character {
                    ' ' | '\t' | '\r' => {}
                    '#' => return true,
                    _ => return true,
                },
                Quote::None => match character {
                    '\'' => {
                        self.quote = Quote::Single;
                        self.at_start = false;
                    }
                    '"' => {
                        self.quote = Quote::Double;
                        self.at_start = false;
                    }
                    '\\' => self.escaped = true,
                    ' ' | '\t' | '\r' => self.expecting_end = true,
                    '#' if self.at_start => return true,
                    _ => self.at_start = false,
                },
            }
        }
        if self.escaped {
            return true;
        }
        matches!(self.quote, Quote::None)
    }
}

/// Parses `source` as a sequence of non-expanding dotenv-style `KEY=VALUE`
/// assignments, validating each key with `validate_name` and stopping once
/// `max_assignments` entries have been accepted.
///
/// Values retain dotenv quoting, escaping, comments, whitespace, and
/// optional `export` support, but `$NAME`/`${NAME}` remain literal — this
/// function never reads the host process environment. Returns entries in
/// file order; a duplicate key is left to the caller (both current callers
/// want "last value wins" applied uniformly with their other local sources,
/// not just within this one file).
pub(crate) fn parse_dotenv(
    source: &str,
    validate_name: impl Fn(&str) -> Result<(), &'static str>,
    max_assignments: usize,
) -> Result<Vec<(String, String)>, DotenvError> {
    let mut entries = Vec::new();
    let mut logical = String::new();
    let mut start_line = 1;
    let mut scan = ValueScan::new();
    for (index, raw_line) in source.split('\n').enumerate() {
        let physical = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if logical.is_empty() {
            start_line = index + 1;
            let trimmed = physical.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((_, input)) = physical.split_once('=') else {
                return Err(DotenvError::Syntax { line: start_line });
            };
            scan = ValueScan::new();
            logical.push_str(physical);
            if !scan.completes_record(input.trim_start()) {
                continue;
            }
        } else {
            logical.push('\n');
            logical.push_str(physical);
            if !scan.completes_record(physical) {
                continue;
            }
        }
        match parse_line(&validate_name, start_line, &logical)? {
            ParsedLine::Skip => {}
            ParsedLine::Entry(key, value) => {
                if entries.len() == max_assignments {
                    return Err(DotenvError::AssignmentLimit);
                }
                entries.push((key, value));
            }
            ParsedLine::Incomplete => return Err(DotenvError::Syntax { line: start_line }),
        }
        logical.clear();
    }
    if !logical.is_empty() {
        return Err(DotenvError::Syntax { line: start_line });
    }
    Ok(entries)
}

fn parse_line(
    validate_name: &impl Fn(&str) -> Result<(), &'static str>,
    line_number: usize,
    line: &str,
) -> Result<ParsedLine, DotenvError> {
    let mut assignment = line.trim_start();
    if assignment.is_empty() || assignment.starts_with('#') {
        return Ok(ParsedLine::Skip);
    }
    if let Some(rest) = assignment.strip_prefix("export")
        && rest.chars().next().is_some_and(char::is_whitespace)
    {
        assignment = rest.trim_start();
    }
    let Some((key, input)) = assignment.split_once('=') else {
        return Err(DotenvError::Syntax { line: line_number });
    };
    let key = key.trim();
    if let Err(reason) = validate_name(key) {
        return Err(DotenvError::InvalidName {
            line: line_number,
            name: key.to_string(),
            reason,
        });
    }
    match parse_value(input.trim_start()) {
        Ok(Some(value)) => Ok(ParsedLine::Entry(key.to_string(), value)),
        Ok(None) => Ok(ParsedLine::Incomplete),
        Err(()) => Err(DotenvError::Syntax { line: line_number }),
    }
}

fn parse_value(input: &str) -> Result<Option<String>, ()> {
    if input.is_empty() || input.starts_with('#') {
        return Ok(Some(String::new()));
    }
    let mut output = String::new();
    let mut quote = Quote::None;
    let mut expecting_end = false;
    let mut chars = input.chars();
    while let Some(character) = chars.next() {
        match quote {
            Quote::Single => {
                if character == '\'' {
                    quote = Quote::None;
                } else {
                    output.push(character);
                }
            }
            Quote::Double => match character {
                '"' => quote = Quote::None,
                '\\' => decode_escape(&mut chars, &mut output)?,
                _ => output.push(character),
            },
            Quote::None if expecting_end => match character {
                ' ' | '\t' | '\r' => {}
                '#' => return Ok(Some(output)),
                _ => return Err(()),
            },
            Quote::None => match character {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => decode_escape(&mut chars, &mut output)?,
                ' ' | '\t' | '\r' => expecting_end = true,
                _ => output.push(character),
            },
        }
    }
    match quote {
        Quote::None => Ok(Some(output)),
        Quote::Single | Quote::Double => Ok(None),
    }
}

fn decode_escape(chars: &mut impl Iterator<Item = char>, output: &mut String) -> Result<(), ()> {
    let Some(escaped) = chars.next() else {
        return Err(());
    };
    match escaped {
        '\\' | '\'' | '"' | '$' | ' ' => output.push(escaped),
        'n' => output.push('\n'),
        _ => return Err(()),
    }
    Ok(())
}
