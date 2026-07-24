//! Runtime `${{ }}` evaluation for strings sourced from an `action.yml`
//! manifest (composite step `if:`/`run:`/`env:`/`with:`, `runs.pre-if`/
//! `post-if`, Docker `args:`/`env:`), rather than from a workflow file.
//!
//! `greenlit-workflow`/`greenlit-engine` only ever plan-fold `${{ }}` text
//! that came from the *workflow* YAML
//! (`greenlit_workflow::model::value::ScalarOrExpr`, folded ahead of time by
//! `greenlit-engine`'s planner into a [`greenlit_engine::pass_through::EnvValue`]).
//! An action's own manifest is a different source a workflow author does
//! not control the planning of, and `greenlit-actions`' manifest parser
//! deliberately preserves this text verbatim rather than plan-folding it
//! (see that crate's `manifest` module docs) — evaluating it is this task
//! group's job, immediately, against the live context, with no static/
//! deferred split to preserve (unlike plan time, every manifest-sourced
//! field this crate resolves is used as a plain string in the end: an env
//! var value, a CLI arg, an image reference, an `if:` truthiness check).

use greenlit_expr::value::{is_truthy, to_display_string};
use greenlit_expr::{Context, evaluate};

/// A manifest-sourced `${{ }}` expression failed, either to parse (this text
/// never went through `greenlit-workflow`'s plan-time parse the way workflow
/// YAML does — see module docs) or to evaluate.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TemplateError {
    /// The expression text itself did not parse.
    #[error("{0}")]
    Parse(#[from] greenlit_expr::ParseError),
    /// The expression parsed but failed to evaluate.
    #[error("{0}")]
    Eval(#[from] greenlit_expr::EvalError),
}

/// Evaluates `raw`, substituting every `${{ expr }}` placeholder with its
/// stringified value; literal text passes through unchanged. A raw value
/// with no `${{` at all (the common case) is returned without invoking the
/// parser.
///
/// # Errors
/// Returns the first [`TemplateError`] a placeholder's expression produces
/// (a parse failure or an evaluation failure).
pub(crate) fn resolve_template(raw: &str, ctx: &Context) -> Result<String, TemplateError> {
    if !raw.contains("${{") {
        return Ok(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        let Some(start) = rest.find("${{") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 3..];
        let Some(end) = find_closing(after_open) else {
            // Unterminated `${{`: keep the remainder verbatim rather than
            // misparsing it as an expression.
            out.push_str(&rest[start..]);
            break;
        };
        let source = after_open[..end].trim();
        let expr = greenlit_expr::parse(source)?;
        let value = evaluate(&expr, ctx)?;
        out.push_str(&to_display_string(&value));
        rest = &after_open[end + 2..];
    }
    Ok(out)
}

/// Evaluates `raw` as an `if:`-style condition (a manifest `pre-if`/
/// `post-if`, or a composite step's own `if:`). GitHub accepts both a bare
/// expression (`runner.os == 'Linux'`) and one wrapped in `${{ }}`
/// (`${{ success() }}`) for these fields, same as a workflow `if:`
/// (`greenlit_engine::condition`'s workflow-level version of this same
/// rule) — this strips a single outer `${{ }}` wrapper when present, then
/// evaluates the remainder as one expression and applies expression
/// truthiness.
///
/// # Errors
/// Returns the underlying [`TemplateError`] on a parse or evaluation failure.
pub(crate) fn resolve_condition(raw: &str, ctx: &Context) -> Result<bool, TemplateError> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
        .map(str::trim)
        .unwrap_or(trimmed);
    let expr = greenlit_expr::parse(inner)?;
    let value = evaluate(&expr, ctx)?;
    Ok(is_truthy(&value))
}

/// Finds the closing `}}` in `text` (which starts just after an opening
/// `${{`), skipping over `}}` sequences inside a single-quoted GitHub
/// expression string literal (where a literal quote is escaped by
/// doubling it) — mirrors `greenlit-engine`'s own template-wrapper scanner
/// (`crate::partial_eval::template`, private to that crate) since action-
/// manifest text needs the identical quote-aware rule and that scanner is
/// not part of any crate's public surface.
fn find_closing(text: &str) -> Option<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use greenlit_expr::{RealFs, Value};
    use std::sync::Arc;

    fn ctx() -> Context {
        Context::new(Arc::new(RealFs::new(std::env::temp_dir())))
            .with_github(Value::object(vec![(
                "repository".to_string(),
                Value::String("octo/hello".to_string()),
            )]))
            .with_env(Value::object(vec![]))
    }

    #[test]
    fn literal_text_with_no_placeholder_passes_through() {
        assert_eq!(
            resolve_template("plain text", &ctx()).unwrap(),
            "plain text"
        );
    }

    #[test]
    fn a_single_placeholder_is_substituted() {
        assert_eq!(
            resolve_template("repo=${{ github.repository }}", &ctx()).unwrap(),
            "repo=octo/hello"
        );
    }

    #[test]
    fn multiple_placeholders_mixed_with_literal_text() {
        assert_eq!(
            resolve_template("[${{ github.repository }}] done", &ctx()).unwrap(),
            "[octo/hello] done"
        );
    }

    #[test]
    fn bare_condition_evaluates_truthily() {
        assert!(resolve_condition("github.repository == 'octo/hello'", &ctx()).unwrap());
        assert!(!resolve_condition("github.repository == 'other'", &ctx()).unwrap());
    }

    #[test]
    fn wrapped_condition_strips_the_delimiters() {
        assert!(resolve_condition("${{ always() }}", &ctx()).unwrap());
    }

    #[test]
    fn closing_delimiter_search_ignores_braces_inside_string_literals() {
        // A `}}` inside a single-quoted string literal must not be mistaken
        // for the wrapper's own closing delimiter; the real one is the
        // `}}` after the closing quote.
        let text = "'contains }} braces' }}";
        let closing = find_closing(text).expect("a real closing delimiter exists");
        assert_eq!(&text[closing..closing + 2], "}}");
        assert_eq!(&text[..closing], "'contains }} braces' ");
    }

    #[test]
    fn closing_delimiter_search_handles_a_doubled_quote_escape() {
        // `''` inside a string literal is GitHub's escape for a literal
        // single quote, not the string's closing quote — a `}}` right
        // after it is still inside the string.
        let text = "'it''s }} fine' }}";
        let closing = find_closing(text).expect("a real closing delimiter exists");
        assert_eq!(&text[closing..closing + 2], "}}");
        assert_eq!(&text[..closing], "'it''s }} fine' ");
    }
}
