//! Delimiter handling for `${{ ... }}` wrappers in workflow scalar text.
//!
//! The complete inner language belongs to `greenlit-expr`; this module only
//! identifies wrapper boundaries before handing the captured body to that
//! crate's public parser.

/// Locate the outer `}}`, ignoring delimiter text inside expression string
/// literals. GitHub expressions allow only single-quoted strings and escape
/// a literal quote by doubling it (`''`); double-quoted strings are invalid:
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#literals>.
///
/// `greenlit-expr`'s public API parses complete expression bodies but does
/// not expose lexer token boundaries, so this state machine performs only
/// wrapper delimiting.
pub(crate) fn find_closing_delimiter(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    let mut in_string = false;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            if in_string && bytes.get(index + 1) == Some(&b'\'') {
                index += 2;
                continue;
            }
            in_string = !in_string;
            index += 1;
            continue;
        }
        if !in_string && bytes[index..].starts_with(b"}}") {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Return the body when `text` consists of exactly one wrapped expression,
/// allowing only surrounding whitespace.
pub(crate) fn single_expression_body(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let after_open = trimmed.strip_prefix("${{")?;
    let end = find_closing_delimiter(after_open)?;
    if !after_open[end + 2..].trim().is_empty() {
        return None;
    }
    Some(after_open[..end].trim())
}
