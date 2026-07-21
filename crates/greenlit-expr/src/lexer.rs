//! Tokenizer for the expression grammar.
//!
//! Source for every rule below: the design memo's "Lexical structure"
//! section, itself derived from `actions/runner`
//! `Sdk/DTExpressions2/Expressions2/Tokens/LexicalAnalyzer.cs` and
//! `Sdk/ExpressionUtility.cs`. This module does not attempt to reproduce
//! GitHub's exact per-token adjacency-table error taxonomy; instead it
//! reproduces the *disambiguation* rules that change what a token stream
//! actually means (dot-vs-number-start, in particular) and leaves "this
//! token sequence has no grammar production" errors to the recursive-descent
//! parser (see `parser` module doc comment).

use crate::error::{MAX_EXPRESSION_LENGTH, ParseError};

/// One lexical token, with the Unicode-scalar offset (not byte offset — the
/// lexer walks `char`s) it started at, for error messages.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TokenSpan {
    pub tok: Tok,
    pub offset: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Star,
    Not,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Null,
    True,
    False,
    Number(f64),
    Str(String),
    /// Any bare identifier that isn't one of the case-sensitive keyword
    /// spellings above. The parser decides Function-vs-NamedValue by
    /// lookahead (does the *next* token open a call?), not the lexer — see
    /// the design memo's note that this is purely a lookahead decision.
    Ident(String),
}

/// Whether the lexer position immediately after the given `prev` token is a
/// "value position" (a primary — literal, named-value, function, `(`, `!`,
/// or a leading `.`/`+`/`-` number — is expected next) as opposed to an
/// "operator position" (a binary operator, `.` dereference, `[` index, `)`,
/// `]`, or `,` is expected next).
///
/// Per the memo: "a leading `.` starts a number only when it appears in
/// value position: expression start, or after `,` `(` `[` or a logical
/// operator; otherwise `.` is dereference." This function generalizes that
/// same value-position test to also gate whether `+`/`-` start a number
/// (there is no arithmetic in this language, so `+`/`-` are *only* ever
/// number prefixes, never binary operators).
fn is_value_position(prev: Option<&Tok>) -> bool {
    match prev {
        None => true,
        // After every binary operator (including relational/equality,
        // where e.g. `x == .5` must lex `.5` as a number): a value is
        // expected next.
        Some(
            Tok::LParen
            | Tok::LBracket
            | Tok::Comma
            | Tok::Not
            | Tok::AndAnd
            | Tok::OrOr
            | Tok::EqEq
            | Tok::NotEq
            | Tok::Lt
            | Tok::Le
            | Tok::Gt
            | Tok::Ge,
        ) => true,
        // After `)`, `]`, `*`, a literal, a dereference `.`, or an
        // identifier (PropertyName/NamedValue): a postfix operator or a
        // binary operator is expected next, *not* a value — critically,
        // this means a subsequent `.` is dereference, not a number start,
        // which is what makes `a.*.b` (wildcard, then further dereference)
        // lex correctly rather than attempting to read `.b` as a malformed
        // number.
        Some(
            Tok::RParen | Tok::RBracket | Tok::Star | Tok::Dot | Tok::Null | Tok::True | Tok::False,
        ) => false,
        Some(Tok::Number(_) | Tok::Str(_) | Tok::Ident(_)) => false,
    }
}

/// Tokenizes a full expression source string (the text already stripped of
/// its `${{ }}` wrapper, if any — see `crate::parse`).
pub(crate) fn tokenize(source: &str) -> Result<Vec<TokenSpan>, ParseError> {
    // C# String.Length, used by the runner's ParseContext, is a count of
    // UTF-16 code units rather than Unicode scalar values. Non-BMP scalars
    // therefore consume two units each.
    // Source: actions/runner ExpressionParser.cs, ParseContext constructor.
    let utf16_len = source.encode_utf16().count();
    if utf16_len > MAX_EXPRESSION_LENGTH {
        return Err(ParseError::TooLong { actual: utf16_len });
    }
    let chars: Vec<char> = source.chars().collect();

    let mut tokens: Vec<TokenSpan> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }

        let value_position = is_value_position(tokens.last().map(|t| &t.tok));
        let start = i;

        // Single-quoted strings: '' is the only escape, no other escapes
        // exist (backslash is a literal character in the source).
        if ch == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                if i >= chars.len() {
                    return Err(ParseError::UnterminatedString { offset: start });
                }
                if chars[i] == '\'' {
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        s.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                s.push(chars[i]);
                i += 1;
            }
            tokens.push(TokenSpan {
                tok: Tok::Str(s),
                offset: start,
            });
            continue;
        }

        // Number tokens: start with a digit, or `+`/`-`/`.` only in value
        // position (a `.`/`+`/`-` in operator position is handled by the
        // punctuation/operator branches below instead).
        let starts_number =
            ch.is_ascii_digit() || ((ch == '+' || ch == '-' || ch == '.') && value_position);
        if starts_number {
            // Greedily consume until a token-boundary character; `.` never
            // terminates a number (so `1.2.3` is consumed as one candidate
            // and fails validation below), per the memo.
            let mut j = i + 1;
            while j < chars.len() && !is_number_boundary(chars[j]) {
                j += 1;
            }
            let text: String = chars[start..j].iter().collect();
            match parse_number(&text) {
                Some(n) => {
                    tokens.push(TokenSpan {
                        tok: Tok::Number(n),
                        offset: start,
                    });
                    i = j;
                    continue;
                }
                None => {
                    return Err(ParseError::InvalidNumber {
                        text,
                        offset: start,
                    });
                }
            }
        }

        // Identifiers and case-sensitive keywords.
        if ch.is_ascii_alphabetic() || ch == '_' {
            let mut j = i + 1;
            while j < chars.len() && is_ident_continue(chars[j]) {
                j += 1;
            }
            let text: String = chars[start..j].iter().collect();
            // Keywords after `.` are never keywords — "an identifier
            // immediately after `.` is always a PropertyName" — so
            // `a.null`/`a.true` are property accesses, not literals. The
            // parser (not the lexer) is what knows whether the previous
            // token was a `.`; reproduce that here by checking directly.
            let prev_was_dot = matches!(tokens.last().map(|t| &t.tok), Some(Tok::Dot));
            let tok = if prev_was_dot {
                Tok::Ident(text)
            } else {
                match text.as_str() {
                    "null" => Tok::Null,
                    "true" => Tok::True,
                    "false" => Tok::False,
                    "NaN" => Tok::Number(f64::NAN),
                    "Infinity" => Tok::Number(f64::INFINITY),
                    _ => Tok::Ident(text),
                }
            };
            tokens.push(TokenSpan { tok, offset: start });
            i = j;
            continue;
        }

        // Punctuation and operators.
        let (tok, len) = match ch {
            '(' => (Tok::LParen, 1),
            ')' => (Tok::RParen, 1),
            '[' => (Tok::LBracket, 1),
            ']' => (Tok::RBracket, 1),
            ',' => (Tok::Comma, 1),
            '.' => (Tok::Dot, 1),
            '*' => (Tok::Star, 1),
            '!' => {
                if peek(&chars, i + 1) == Some('=') {
                    (Tok::NotEq, 2)
                } else {
                    (Tok::Not, 1)
                }
            }
            '<' => {
                if peek(&chars, i + 1) == Some('=') {
                    (Tok::Le, 2)
                } else {
                    (Tok::Lt, 1)
                }
            }
            '>' => {
                if peek(&chars, i + 1) == Some('=') {
                    (Tok::Ge, 2)
                } else {
                    (Tok::Gt, 1)
                }
            }
            '=' => {
                if peek(&chars, i + 1) == Some('=') {
                    (Tok::EqEq, 2)
                } else {
                    return Err(ParseError::UnexpectedChar { ch, offset: start });
                }
            }
            '&' => {
                if peek(&chars, i + 1) == Some('&') {
                    (Tok::AndAnd, 2)
                } else {
                    return Err(ParseError::UnexpectedChar { ch, offset: start });
                }
            }
            '|' => {
                if peek(&chars, i + 1) == Some('|') {
                    (Tok::OrOr, 2)
                } else {
                    return Err(ParseError::UnexpectedChar { ch, offset: start });
                }
            }
            other => {
                return Err(ParseError::UnexpectedChar {
                    ch: other,
                    offset: start,
                });
            }
        };
        tokens.push(TokenSpan { tok, offset: start });
        i += len;
    }

    Ok(tokens)
}

fn peek(chars: &[char], at: usize) -> Option<char> {
    chars.get(at).copied()
}

fn is_ident_continue(ch: char) -> bool {
    // `[a-zA-Z_][a-zA-Z0-9_-]*` — note `-` is allowed after the first
    // character (this is why `setup-node`-style identifiers lex correctly
    // as a single NamedValue/PropertyName token).
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

fn is_number_boundary(ch: char) -> bool {
    matches!(
        ch,
        '(' | ')' | '[' | ']' | ',' | '!' | '<' | '>' | '=' | '&' | '|'
    ) || ch.is_whitespace()
}

/// Parses a number-token candidate string under GitHub's exact rules (the
/// design memo's ToNumber/lexer section): JSON-ish decimal (including
/// lenient `.5`, `1.`, `+3`), `0x`/`0o` literals (`i32` range only), and the
/// exact-ordinal `Infinity`/`-Infinity` forms. The radix prefixes are
/// lowercase-only because `ExpressionUtility.ParseNumber` checks `str[1]`
/// against lowercase `x`/`o` exactly. Returns `None` (⇒ invalid
/// token) if nothing matches, mirroring "if it parses to NaN the token is
/// invalid".
///
/// Source: <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/ExpressionUtility.cs>.
pub(crate) fn parse_number(text: &str) -> Option<f64> {
    if text == "Infinity" {
        return Some(f64::INFINITY);
    }
    if text == "-Infinity" {
        return Some(f64::NEG_INFINITY);
    }
    if let Some(hex) = text.strip_prefix("0x") {
        if !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return i32::from_str_radix(hex, 16).ok().map(f64::from);
        }
        return None;
    }
    if let Some(oct) = text.strip_prefix("0o") {
        if !oct.is_empty() && oct.chars().all(|c| ('0'..='7').contains(&c)) {
            return i32::from_str_radix(oct, 8).ok().map(f64::from);
        }
        return None;
    }
    // Decimal: reject anything Rust's own parser is *more* lenient about
    // than GitHub (e.g. Rust accepts "inf"/"infinity"/"nan" case-insensitively
    // as f64 literals, which would wrongly accept e.g. "infcheck"-shaped
    // rejects — guard with an explicit character-class check first).
    if text.is_empty() {
        return None;
    }
    let digits_part = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits_part.is_empty()
        || !digits_part
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-')
    {
        return None;
    }
    // Must contain at least one digit somewhere (rejects "." / "+" / "-e5").
    if !digits_part.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse::<f64>().ok().filter(|n| !n.is_nan())
}
