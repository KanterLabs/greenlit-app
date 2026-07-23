//! Static expression-path resolution used by dependency classification.

use greenlit_expr::value::to_display_string;
use greenlit_expr::{Expr, Value};

use crate::partial_eval::{FoldCtx, Folded, PartialEvalError, fold_expr};

pub(super) fn expression_root(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::NamedValue(name) => Some(name),
        Expr::Index { target, .. } | Expr::Wildcard { target } => expression_root(target),
        _ => None,
    }
}

/// Resolves a property chain whose indexes are literals or fully static
/// scalar expressions. This lets `strategy[format('{0}', 'job-total')]`
/// receive the same fine-grained classification as dot syntax instead of
/// deferring the entire strategy context. The same rule resolves a computed
/// `env[...]` key to one exact name; only a runtime-dependent or non-scalar
/// key retains wildcard uncertainty.
pub(super) fn static_path(
    expr: &Expr,
    ctx: &FoldCtx<'_>,
) -> Result<Option<(String, Vec<String>)>, PartialEvalError> {
    fn walk(
        expr: &Expr,
        ctx: &FoldCtx<'_>,
        segments: &mut Vec<String>,
    ) -> Result<Option<String>, PartialEvalError> {
        match expr {
            Expr::NamedValue(name) => Ok(Some(name.clone())),
            Expr::Index { target, index } => {
                let Some(root) = walk(target, ctx, segments)? else {
                    return Ok(None);
                };
                if path_target_is_known_primitive_or_missing(&root, segments, ctx) {
                    // Retain the reachable path for precise dependencies,
                    // but skip every key after a primitive target.
                    // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Operators/Index.cs
                    return Ok(Some(root));
                }
                if matches!(
                    fold_expr(target, ctx)?,
                    Folded::Value(
                        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                    )
                ) {
                    return Ok(Some(root));
                }
                let segment = match index.as_ref() {
                    Expr::Str(value) => Some(value.clone()),
                    other => match fold_expr(other, ctx)? {
                        Folded::Value(
                            value @ (Value::Null
                            | Value::Bool(_)
                            | Value::Number(_)
                            | Value::String(_)),
                        ) => Some(to_display_string(&value)),
                        Folded::Value(Value::Array(_) | Value::Object(_))
                        | Folded::Residual { .. } => None,
                    },
                };
                if let Some(segment) = segment {
                    segments.push(segment);
                    Ok(Some(root))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }
    let mut segments = Vec::new();
    let Some(root) = walk(expr, ctx, &mut segments)? else {
        return Ok(None);
    };
    Ok(Some((root, segments)))
}

/// Whether a resolved fixed-shape `needs`/`steps` path is guaranteed to be a
/// primitive or GitHub's empty-string missing-property value.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#needs-context>
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#steps-context>
fn path_target_is_known_primitive_or_missing(
    root: &str,
    segments: &[String],
    ctx: &FoldCtx<'_>,
) -> bool {
    if root.eq_ignore_ascii_case("github") {
        segments.first().is_some_and(|property| {
            ctx.roots
                .github_deferred
                .iter()
                .any(|deferred| deferred.eq_ignore_ascii_case(property))
        })
    } else if root.eq_ignore_ascii_case("needs") {
        let Some(job) = segments.first() else {
            return false;
        };
        if ctx.roots.needs_slots.canonical_job_id(job).is_none() {
            return true;
        }
        match segments.get(1) {
            None => false,
            Some(field) if field.eq_ignore_ascii_case("outputs") => segments.len() > 2,
            Some(_) => true,
        }
    } else if root.eq_ignore_ascii_case("steps") {
        let Some(step) = segments.first() else {
            return false;
        };
        if !ctx.roots.steps_slots.contains(step) {
            return true;
        }
        match segments.get(1) {
            None => false,
            Some(field) if field.eq_ignore_ascii_case("outputs") => segments.len() > 2,
            Some(_) => true,
        }
    } else {
        false
    }
}
