//! Folds a raw template string (an `env:` entry or a job output value:
//! zero or more `${{ }}` placeholders, possibly mixed with literal text)
//! against a [`FoldCtx`], reusing [`fold_expr`] per placeholder — see the
//! parent module's doc comment.

use std::collections::BTreeSet;

use greenlit_expr::Expr;
use greenlit_expr::value::to_display_string;

use super::fold::fold_expr;
use super::printer::value_to_literal_expr;
use super::{FoldCtx, Folded, PartialEvalError, TemplateFold};

enum TemplatePart<'a> {
    Literal(&'a str),
    Placeholder(&'a str),
}

/// Splits `raw` into literal-text and `${{ ... }}`-placeholder-source
/// segments, in order.
fn split_template(raw: &str) -> Vec<TemplatePart<'_>> {
    let mut parts = Vec::new();
    let mut rest = raw;
    loop {
        match rest.find("${{") {
            None => {
                if !rest.is_empty() {
                    parts.push(TemplatePart::Literal(rest));
                }
                break;
            }
            Some(start) => {
                if start > 0 {
                    parts.push(TemplatePart::Literal(&rest[..start]));
                }
                let after_open = &rest[start + 3..];
                match after_open.find("}}") {
                    None => {
                        // Unterminated `${{`: `greenlit-workflow` already
                        // requires balanced delimiters for anything it
                        // classifies as an `Expression`; stay total by
                        // treating the remainder as literal rather than
                        // panicking.
                        parts.push(TemplatePart::Literal(rest));
                        break;
                    }
                    Some(end) => {
                        parts.push(TemplatePart::Placeholder(after_open[..end].trim()));
                        rest = &after_open[end + 2..];
                    }
                }
            }
        }
    }
    parts
}

fn escape_format_braces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '{' => out.push_str("{{"),
            '}' => out.push_str("}}"),
            _ => out.push(c),
        }
    }
    out
}

/// Folds a raw template string (an `env:` entry or a job output value: zero
/// or more `${{ }}` placeholders, possibly mixed with literal text) against
/// `ctx`.
pub(crate) fn fold_template(
    raw: &str,
    ctx: &FoldCtx<'_>,
) -> Result<TemplateFold, PartialEvalError> {
    let parts = split_template(raw);
    let placeholder_count = parts
        .iter()
        .filter(|p| matches!(p, TemplatePart::Placeholder(_)))
        .count();

    if placeholder_count == 0 {
        return Ok(TemplateFold::Static(greenlit_expr::Value::String(
            raw.to_string(),
        )));
    }

    // Exactly one whole-template placeholder: preserve its native type
    // (design memo, `ScalarOrExpr` doc comment's "preserves the
    // expression's own type" rule).
    if placeholder_count == 1
        && parts.len() == 1
        && let TemplatePart::Placeholder(src) = parts[0]
    {
        let expr = greenlit_expr::parse(src)?;
        return Ok(match fold_expr(&expr, ctx)? {
            Folded::Value(v) => TemplateFold::Static(v),
            Folded::Residual { expr, defers_on } => {
                // `residual_text` shows just the (possibly partially
                // folded) expression, without the `${{ }}` wrapper —
                // `source` (kept separately by the caller, e.g.
                // `PlannedOutput::source`) is what still carries the
                // wrapper (design memo §4.4's worked JSON example).
                let residual_text = super::pretty_print(&expr);
                TemplateFold::Deferred {
                    residual: expr,
                    residual_text,
                    defers_on,
                }
            }
        });
    }

    let mut folded_each: Vec<Folded> = Vec::with_capacity(placeholder_count);
    for part in &parts {
        if let TemplatePart::Placeholder(src) = part {
            let expr = greenlit_expr::parse(src)?;
            folded_each.push(fold_expr(&expr, ctx)?);
        }
    }

    if folded_each.iter().all(|f| matches!(f, Folded::Value(_))) {
        let mut out = String::new();
        let mut it = folded_each.iter();
        for part in &parts {
            match part {
                TemplatePart::Literal(lit) => out.push_str(lit),
                TemplatePart::Placeholder(_) => {
                    if let Some(Folded::Value(v)) = it.next() {
                        out.push_str(&to_display_string(v));
                    }
                }
            }
        }
        return Ok(TemplateFold::Static(greenlit_expr::Value::String(out)));
    }

    // At least one placeholder is deferred: `residual_text` substitutes
    // only the resolved placeholders (matching the design memo's own
    // worked example, "`v1.4-${{ steps.meta.outputs.sha }}` where only the
    // literal prefix is known"); `residual` reconstructs the whole template
    // as a `format()` call (GitHub's own positional-template function) —
    // see `value_to_literal_expr`'s doc comment for why this reconstruction
    // only needs to be *valid*, not literally what the user wrote.
    let mut residual_text = String::new();
    let mut pattern = String::new();
    let mut call_args: Vec<Expr> = Vec::new();
    let mut defers = BTreeSet::new();
    let mut it = folded_each.into_iter();
    for part in &parts {
        match part {
            TemplatePart::Literal(lit) => {
                residual_text.push_str(lit);
                pattern.push_str(&escape_format_braces(lit));
            }
            TemplatePart::Placeholder(src) => {
                let Some(folded) = it.next() else { continue };
                let idx = call_args.len();
                pattern.push('{');
                pattern.push_str(&idx.to_string());
                pattern.push('}');
                match folded {
                    Folded::Value(v) => {
                        residual_text.push_str(&to_display_string(&v));
                        call_args.push(value_to_literal_expr(&v));
                    }
                    Folded::Residual { expr, defers_on } => {
                        residual_text.push_str("${{ ");
                        residual_text.push_str(src);
                        residual_text.push_str(" }}");
                        call_args.push(expr);
                        defers.extend(defers_on);
                    }
                }
            }
        }
    }
    let mut full_args = vec![Expr::Str(pattern)];
    full_args.extend(call_args);
    Ok(TemplateFold::Deferred {
        residual: Expr::Call {
            name: "format".to_string(),
            args: full_args,
        },
        residual_text,
        defers_on: defers,
    })
}
