//! Folds a raw template string (an `env:` entry or a job output value:
//! zero or more `${{ }}` placeholders, possibly mixed with literal text)
//! against a [`FoldCtx`], reusing [`fold_expr`] per placeholder — see the
//! parent module's doc comment.

use std::collections::BTreeSet;

use greenlit_expr::Expr;
use greenlit_expr::value::to_display_string;

use super::fold::fold_expr;
use super::{FoldCtx, Folded, PartialEvalError, TemplateFold};

enum TemplatePart<'a> {
    Literal(&'a str),
    Placeholder(&'a str),
}

enum PlannedTemplatePart<'a> {
    Literal(&'a str),
    Static(String),
    Deferred {
        source: &'a str,
        expr: Expr,
        defers_on: BTreeSet<crate::defer::DeferReason>,
    },
}

/// Mirrors the node-local memory counter used by the runner's synthetic
/// `format()` expression for a mixed scalar. Each nonempty argument or
/// literal output segment costs 26 bytes plus two per UTF-16 code unit;
/// empty arguments cost nothing. An escaped brace ends the current literal
/// segment, so each literal brace can add another 26-byte string overhead.
///
/// <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/Expressions/Sdk/Functions/Format.cs#L214-L267>
/// <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/Expressions/Sdk/MemoryCounter.cs#L45-L57>
struct TemplateStringBudget {
    bytes: usize,
}

impl TemplateStringBudget {
    fn new() -> Self {
        Self { bytes: 0 }
    }

    fn add_literal(&mut self, text: &str) -> Result<(), PartialEvalError> {
        let mut segment_start = 0usize;
        for (index, character) in text.char_indices() {
            if character == '{' || character == '}' {
                let segment_end = index + character.len_utf8();
                self.add_output_segment(&text[segment_start..segment_end])?;
                segment_start = segment_end;
            }
        }
        self.add_output_segment(&text[segment_start..])
    }

    fn add_argument(&mut self, text: &str) -> Result<(), PartialEvalError> {
        self.add_output_segment(text)
    }

    fn add_output_segment(&mut self, text: &str) -> Result<(), PartialEvalError> {
        if text.is_empty() {
            return Ok(());
        }
        let payload_bytes = text
            .encode_utf16()
            .count()
            .checked_mul(2)
            .ok_or_else(template_memory_error)?;
        let added = 26usize
            .checked_add(payload_bytes)
            .ok_or_else(template_memory_error)?;
        self.bytes = self
            .bytes
            .checked_add(added)
            .ok_or_else(template_memory_error)?;
        if self.bytes > greenlit_expr::WORKFLOW_TEMPLATE_MAX_MEMORY_BYTES {
            return Err(template_memory_error());
        }
        Ok(())
    }
}

fn template_memory_error() -> PartialEvalError {
    greenlit_expr::EvalError::MemoryLimitExceeded {
        max_bytes: greenlit_expr::WORKFLOW_TEMPLATE_MAX_MEMORY_BYTES,
    }
    .into()
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
                match find_closing_delimiter(after_open) {
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

/// Finds the closing wrapper delimiter while ignoring `}}` inside GitHub
/// expression string literals. Expression strings are single-quoted and a
/// quote inside a string is escaped by doubling it.
fn find_closing_delimiter(text: &str) -> Option<usize> {
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

    // Exactly one whole-template placeholder preserves its native type,
    // matching GitHub's documented expression literal types.
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
                // wrapper.
                let residual_text = super::pretty_print(&expr);
                TemplateFold::Deferred {
                    residual: expr,
                    residual_text,
                    defers_on,
                }
            }
        });
    }

    let mut output_budget = TemplateStringBudget::new();
    let mut planned_parts = Vec::with_capacity(parts.len());
    let mut has_deferred = false;
    for part in parts {
        match part {
            TemplatePart::Literal(literal) => {
                output_budget.add_literal(literal)?;
                planned_parts.push(PlannedTemplatePart::Literal(literal));
            }
            TemplatePart::Placeholder(source) => {
                let expr = greenlit_expr::parse(source)?;
                match fold_expr(&expr, ctx)? {
                    Folded::Value(value) => {
                        // A mixed template stringifies each placeholder. Keep
                        // only that bounded rendering instead of retaining a
                        // potentially large array/object result per repeat.
                        let display = to_display_string(&value);
                        output_budget.add_argument(&display)?;
                        planned_parts.push(PlannedTemplatePart::Static(display));
                    }
                    Folded::Residual { expr, defers_on } => {
                        has_deferred = true;
                        planned_parts.push(PlannedTemplatePart::Deferred {
                            source,
                            expr,
                            defers_on,
                        });
                    }
                }
            }
        }
    }

    if !has_deferred {
        let mut out = String::new();
        for part in planned_parts {
            match part {
                PlannedTemplatePart::Literal(literal) => out.push_str(literal),
                PlannedTemplatePart::Static(value) => out.push_str(&value),
                PlannedTemplatePart::Deferred { .. } => {
                    // `has_deferred` is set in the only constructor for this
                    // variant, so this arm is structurally unreachable. Keep
                    // the function total if that invariant changes.
                    return Err(template_memory_error());
                }
            }
        }
        return Ok(TemplateFold::Static(greenlit_expr::Value::String(out)));
    }

    // At least one placeholder is deferred: `residual_text` substitutes
    // only the resolved placeholders; `residual` reconstructs the whole
    // template
    // as a `format()` call (GitHub's own positional-template function) —
    // see `value_to_literal_expr`'s doc comment for why this reconstruction
    // only needs to be *valid*, not literally what the user wrote.
    let mut residual_text = String::new();
    let mut pattern = String::new();
    let mut call_args: Vec<Expr> = Vec::new();
    let mut defers = BTreeSet::new();
    for part in planned_parts {
        match part {
            PlannedTemplatePart::Literal(literal) => {
                residual_text.push_str(literal);
                let escaped = escape_format_braces(literal);
                pattern.push_str(&escaped);
            }
            PlannedTemplatePart::Static(value) => {
                let idx = call_args.len();
                let placeholder = format!("{{{idx}}}");
                pattern.push_str(&placeholder);
                residual_text.push_str(&value);
                call_args.push(Expr::Str(value));
            }
            PlannedTemplatePart::Deferred {
                source,
                expr,
                defers_on,
            } => {
                let idx = call_args.len();
                let placeholder = format!("{{{idx}}}");
                pattern.push_str(&placeholder);
                residual_text.push_str("${{ ");
                residual_text.push_str(source);
                residual_text.push_str(" }}");
                call_args.push(expr);
                defers.extend(defers_on);
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
