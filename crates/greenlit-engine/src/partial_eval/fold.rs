//! The recursive expression folder — see the parent module's doc comment.

use std::collections::BTreeSet;

use greenlit_expr::value::{
    abstract_equal, abstract_not_equal, greater_or_equal, greater_than, is_falsy, is_truthy,
    less_or_equal, less_than,
};
use greenlit_expr::{BinOp, Expr, Value};

use super::calls::{fold_call, is_partially_foldable_function};
use super::classify::collect_defer_reasons;
use super::{FoldCtx, Folded, PartialEvalError, value_to_literal_expr};
use crate::defer::DeferReason;

/// Folds a bare `${{ }}`-wrapper-stripped expression (an `if:` condition,
/// or one placeholder inside a template) against `ctx`.
pub(crate) fn fold_expr(expr: &Expr, ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    let mut defers = BTreeSet::new();
    collect_defer_reasons(expr, ctx, &mut defers)?;
    if defers.is_empty() {
        // Workflow-template evaluation supplies the surrounding template's
        // 10 MiB budget instead of the expression SDK's standalone 1 MiB
        // default. This is the runner's `TemplateToken` call path.
        // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTObjectTemplating/ObjectTemplating/Tokens/TemplateToken.cs#L52-L65
        let value = greenlit_expr::evaluate_with_options(
            expr,
            &ctx.static_context(),
            greenlit_expr::EvaluationOptions::new(
                greenlit_expr::WORKFLOW_TEMPLATE_MAX_MEMORY_BYTES,
            ),
        )?;
        return Ok(Folded::Value(value));
    }
    match expr {
        Expr::Binary {
            op: BinOp::And,
            lhs,
            rhs,
        } => fold_and(lhs, rhs, ctx),
        Expr::Binary {
            op: BinOp::Or,
            lhs,
            rhs,
        } => fold_or(lhs, rhs, ctx),
        Expr::Not(inner) => fold_not(inner, ctx),
        Expr::Binary { op, lhs, rhs } => fold_strict_binary(*op, lhs, rhs, ctx),
        Expr::Call { name, args } if is_partially_foldable_function(name) => {
            fold_call(name, args, ctx)
        }
        _ => Ok(Folded::Residual {
            expr: expr.clone(),
            defers_on: defers.clone(),
        }),
    }
}

pub(super) fn split_folded(f: Folded) -> (Expr, BTreeSet<DeferReason>) {
    match f {
        Folded::Value(v) => (value_to_literal_expr(&v), BTreeSet::new()),
        Folded::Residual { expr, defers_on } => (expr, defers_on),
    }
}

/// `!x` always produces a genuine boolean, so a static operand folds all
/// the way while a deferred operand carries the `!` through.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#operators>
fn fold_not(inner: &Expr, ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    match fold_expr(inner, ctx)? {
        Folded::Value(v) => Ok(Folded::Value(Value::Bool(is_falsy(&v)))),
        Folded::Residual { expr, defers_on } => Ok(Folded::Residual {
            expr: Expr::Not(Box::new(expr)),
            defers_on,
        }),
    }
}

/// Produces the locally folded shape for `a && b` while preserving GitHub's
/// left-to-right short-circuit behavior. A decisive static left operand
/// removes the unreachable right branch. If the left operand is deferred,
/// the right branch stays unevaluated until runtime because evaluating a
/// static-but-fallible call there (for example `fromJSON`) could raise an
/// error that GitHub would never reach.
fn fold_and(lhs: &Expr, rhs: &Expr, ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    match fold_expr(lhs, ctx)? {
        Folded::Value(l) => {
            if is_falsy(&l) {
                Ok(Folded::Value(l))
            } else {
                fold_expr(rhs, ctx)
            }
        }
        Folded::Residual {
            expr: lhs_expr,
            defers_on: lhs_defers,
        } => {
            let mut rhs_defers = BTreeSet::new();
            collect_defer_reasons(rhs, ctx, &mut rhs_defers)?;
            let mut defers = lhs_defers;
            defers.extend(rhs_defers);
            Ok(Folded::Residual {
                expr: Expr::Binary {
                    op: BinOp::And,
                    lhs: Box::new(lhs_expr),
                    rhs: Box::new(rhs.clone()),
                },
                defers_on: defers,
            })
        }
    }
}

/// `a || b`: the truthy-mirror of [`fold_and`].
fn fold_or(lhs: &Expr, rhs: &Expr, ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    match fold_expr(lhs, ctx)? {
        Folded::Value(l) => {
            if is_truthy(&l) {
                Ok(Folded::Value(l))
            } else {
                fold_expr(rhs, ctx)
            }
        }
        Folded::Residual {
            expr: lhs_expr,
            defers_on: lhs_defers,
        } => {
            let mut rhs_defers = BTreeSet::new();
            collect_defer_reasons(rhs, ctx, &mut rhs_defers)?;
            let mut defers = lhs_defers;
            defers.extend(rhs_defers);
            Ok(Folded::Residual {
                expr: Expr::Binary {
                    op: BinOp::Or,
                    lhs: Box::new(lhs_expr),
                    rhs: Box::new(rhs.clone()),
                },
                defers_on: defers,
            })
        }
    }
}

/// `==`/`!=`/`<`/`<=`/`>`/`>=`: unlike `&&`/`||` these are not
/// short-circuiting — both operands are always needed, so either being
/// deferred defers the whole comparison.
fn fold_strict_binary(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    ctx: &FoldCtx<'_>,
) -> Result<Folded, PartialEvalError> {
    let l = fold_expr(lhs, ctx)?;
    let r = fold_expr(rhs, ctx)?;
    if let (Folded::Value(lv), Folded::Value(rv)) = (&l, &r) {
        return Ok(Folded::Value(Value::Bool(apply_strict_op(op, lv, rv))));
    }
    let (lexpr, ldef) = split_folded(l);
    let (rexpr, rdef) = split_folded(r);
    let mut defers = ldef;
    defers.extend(rdef);
    Ok(Folded::Residual {
        expr: Expr::Binary {
            op,
            lhs: Box::new(lexpr),
            rhs: Box::new(rexpr),
        },
        defers_on: defers,
    })
}

fn apply_strict_op(op: BinOp, l: &Value, r: &Value) -> bool {
    match op {
        BinOp::Eq => abstract_equal(l, r),
        BinOp::NotEq => abstract_not_equal(l, r),
        BinOp::Lt => less_than(l, r),
        BinOp::Le => less_or_equal(l, r),
        BinOp::Gt => greater_than(l, r),
        BinOp::Ge => greater_or_equal(l, r),
        // `fold_expr` only reaches `fold_strict_binary` via its catch-all
        // `Expr::Binary { op, .. }` arm, which is tried only *after* the
        // `BinOp::And`/`BinOp::Or` arms above it — so this arm is
        // unreachable in practice. Kept total (rather than panicking) per
        // the no-`unwrap`/`expect`/`panic!` quality bar.
        BinOp::And | BinOp::Or => false,
    }
}
