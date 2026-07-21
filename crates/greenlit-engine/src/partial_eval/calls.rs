//! Partial folding for built-in expression functions.

use std::collections::BTreeSet;

use greenlit_expr::{Expr, Value};

use super::classify::collect_defer_reasons;
use super::fold::{fold_expr, split_folded};
use super::{FoldCtx, Folded, PartialEvalError, value_to_literal_expr};
use crate::defer::DeferReason;

/// Partially folds a built-in while preserving the runner's documented
/// left-to-right and branch-selective argument evaluation. In particular,
/// an unused `format` value, a guarded `contains` item, a short `join`
/// separator, or an unselected `case` branch must not raise a plan-time
/// error.
pub(super) fn fold_call(
    name: &str,
    args: &[Expr],
    ctx: &FoldCtx<'_>,
) -> Result<Folded, PartialEvalError> {
    match name.to_ascii_lowercase().as_str() {
        "contains" | "startswith" | "endswith" => fold_guarded_binary_call(name, args, ctx),
        "format" => fold_format(args, ctx),
        "join" => fold_join(args, ctx),
        "case" => fold_case(args, ctx),
        "tojson" | "fromjson" => fold_all_args(name, args, ctx),
        _ => residual_call(name, args.to_vec(), collect_call_reasons(args, ctx)?),
    }
}

fn fold_all_args(name: &str, args: &[Expr], ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    let mut arg_exprs = Vec::with_capacity(args.len());
    let mut defers = BTreeSet::new();
    for a in args {
        let (e, d) = split_folded(fold_expr(a, ctx)?);
        arg_exprs.push(e);
        defers.extend(d);
    }
    finish_call(name, arg_exprs, defers, ctx)
}

fn fold_guarded_binary_call(
    name: &str,
    args: &[Expr],
    ctx: &FoldCtx<'_>,
) -> Result<Folded, PartialEvalError> {
    let first = fold_expr(&args[0], ctx)?;
    match first {
        Folded::Value(value) if guarded_call_skips_second(name, &value) => evaluate_call(
            name,
            vec![value_to_literal_expr(&value), args[1].clone()],
            ctx,
        ),
        Folded::Value(value) => {
            let second = fold_expr(&args[1], ctx)?;
            let (second_expr, defers) = split_folded(second);
            finish_call(
                name,
                vec![value_to_literal_expr(&value), second_expr],
                defers,
                ctx,
            )
        }
        Folded::Residual {
            expr,
            mut defers_on,
        } => {
            defers_on.extend(collect_call_reasons(&args[1..], ctx)?);
            residual_call(name, vec![expr, args[1].clone()], defers_on)
        }
    }
}

fn guarded_call_skips_second(name: &str, first: &Value) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "contains" => {
            matches!(first, Value::Object(_))
                || matches!(first, Value::Array(array) if array.is_empty())
        }
        "startswith" | "endswith" => matches!(first, Value::Array(_) | Value::Object(_)),
        _ => false,
    }
}

fn fold_join(args: &[Expr], ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    let first = fold_expr(&args[0], ctx)?;
    match first {
        Folded::Value(value) => {
            let needs_separator =
                matches!(&value, Value::Array(array) if array.len() >= 2) && args.len() > 1;
            if !needs_separator {
                return evaluate_call(
                    "join",
                    std::iter::once(value_to_literal_expr(&value))
                        .chain(args.get(1).cloned())
                        .collect(),
                    ctx,
                );
            }
            let separator = fold_expr(&args[1], ctx)?;
            let (separator_expr, defers) = split_folded(separator);
            finish_call(
                "join",
                vec![value_to_literal_expr(&value), separator_expr],
                defers,
                ctx,
            )
        }
        Folded::Residual {
            expr,
            mut defers_on,
        } => {
            defers_on.extend(collect_call_reasons(&args[1..], ctx)?);
            let call_args = std::iter::once(expr).chain(args.get(1).cloned()).collect();
            residual_call("join", call_args, defers_on)
        }
    }
}

fn fold_format(args: &[Expr], ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    let pattern = fold_expr(&args[0], ctx)?;
    let Folded::Value(pattern) = pattern else {
        let (pattern, mut defers) = split_folded(pattern);
        defers.extend(collect_call_reasons(&args[1..], ctx)?);
        let call_args = std::iter::once(pattern)
            .chain(args[1..].iter().cloned())
            .collect();
        return residual_call("format", call_args, defers);
    };

    let pattern_text = greenlit_expr::value::to_display_string(&pattern);
    let mut scanner =
        greenlit_expr::functions::format::FormatScanner::new(&pattern_text, args.len() - 1);
    let mut call_args = std::iter::once(value_to_literal_expr(&pattern))
        .chain(args[1..].iter().cloned())
        .collect::<Vec<_>>();
    let mut seen = vec![false; args.len() - 1];
    let mut defers = BTreeSet::new();
    let mut encountered_deferred = false;
    loop {
        let token = match scanner.next_token() {
            Ok(token) => token,
            Err(_) if encountered_deferred => break,
            Err(error) => return Err(greenlit_expr::EvalError::from(error).into()),
        };
        let Some(token) = token else { break };
        let greenlit_expr::functions::format::FormatToken::Placeholder { index, spec } = token
        else {
            continue;
        };
        if !seen[index] {
            seen[index] = true;
            if encountered_deferred {
                collect_defer_reasons(&args[index + 1], ctx, &mut defers)?;
            } else {
                let folded = fold_expr(&args[index + 1], ctx)?;
                encountered_deferred = matches!(folded, Folded::Residual { .. });
                let (expr, argument_defers) = split_folded(folded);
                call_args[index + 1] = expr;
                defers.extend(argument_defers);
            }
        }
        if !spec.is_empty() {
            if encountered_deferred {
                break;
            }
            return evaluate_call("format", call_args, ctx);
        }
    }
    finish_call("format", call_args, defers, ctx)
}

fn fold_case(args: &[Expr], ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    let pairs_end = args.len() - 1;
    for pair_start in (0..pairs_end).step_by(2) {
        match fold_expr(&args[pair_start], ctx)? {
            Folded::Value(Value::Bool(false)) => {}
            Folded::Value(Value::Bool(true)) => return fold_expr(&args[pair_start + 1], ctx),
            Folded::Value(value) => {
                return Err(greenlit_expr::EvalError::InvalidCasePredicate {
                    position: pair_start / 2 + 1,
                    kind: value.kind(),
                }
                .into());
            }
            Folded::Residual {
                expr,
                mut defers_on,
            } => {
                defers_on.extend(collect_call_reasons(&args[pair_start + 1..], ctx)?);
                let remaining = std::iter::once(expr)
                    .chain(args[pair_start + 1..].iter().cloned())
                    .collect();
                return residual_call("case", remaining, defers_on);
            }
        }
    }
    fold_expr(&args[pairs_end], ctx)
}

fn finish_call(
    name: &str,
    args: Vec<Expr>,
    defers: BTreeSet<DeferReason>,
    ctx: &FoldCtx<'_>,
) -> Result<Folded, PartialEvalError> {
    if defers.is_empty() {
        evaluate_call(name, args, ctx)
    } else {
        residual_call(name, args, defers)
    }
}

fn evaluate_call(
    name: &str,
    args: Vec<Expr>,
    ctx: &FoldCtx<'_>,
) -> Result<Folded, PartialEvalError> {
    let expression = Expr::Call {
        name: name.to_string(),
        args,
    };
    Ok(Folded::Value(greenlit_expr::evaluate(
        &expression,
        &ctx.static_context(),
    )?))
}

fn residual_call(
    name: &str,
    args: Vec<Expr>,
    defers_on: BTreeSet<DeferReason>,
) -> Result<Folded, PartialEvalError> {
    Ok(Folded::Residual {
        expr: Expr::Call {
            name: name.to_string(),
            args,
        },
        defers_on,
    })
}

fn collect_call_reasons(
    args: &[Expr],
    ctx: &FoldCtx<'_>,
) -> Result<BTreeSet<DeferReason>, PartialEvalError> {
    let mut defers = BTreeSet::new();
    for argument in args {
        collect_defer_reasons(argument, ctx, &mut defers)?;
    }
    Ok(defers)
}

pub(super) fn is_partially_foldable_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "contains" | "startswith" | "endswith" | "format" | "join" | "tojson" | "fromjson" | "case"
    )
}
