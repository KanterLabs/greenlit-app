//! The recursive folder ([`fold_expr`]) and its [`DeferReason`]
//! classification pass — see the parent module's doc comment.

use std::collections::BTreeSet;

use greenlit_expr::value::{
    abstract_equal, abstract_not_equal, greater_or_equal, greater_than, is_falsy, is_truthy,
    less_or_equal, less_than,
};
use greenlit_expr::{BinOp, Expr, Value};

use super::{FoldCtx, Folded, PartialEvalError, value_to_literal_expr};
use crate::defer::{DeferReason, StatusFn, StepStatusField};
use crate::graph::JobId;

/// Folds a bare `${{ }}`-wrapper-stripped expression (an `if:` condition,
/// or one placeholder inside a template) against `ctx`.
pub(crate) fn fold_expr(expr: &Expr, ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    let mut defers = BTreeSet::new();
    collect_defer_reasons(expr, ctx, &mut defers)?;
    if defers.is_empty() {
        let value = greenlit_expr::evaluate(expr, &ctx.static_context())?;
        return Ok(Folded::Value(value));
    }
    let folded = match expr {
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
        Expr::Call { name, args } if is_pure_function(name) => fold_pure_call(name, args, ctx),
        _ => Ok(Folded::Residual {
            expr: expr.clone(),
            defers_on: defers.clone(),
        }),
    }?;

    // `needs.*` and `steps.*` are populated as jobs/steps complete. Even
    // when a currently-known sibling would short-circuit `&&`/`||`, the
    // authored expression must remain available for runtime evaluation;
    // plan-time folding must never erase a runtime context dependency.
    // GitHub docs: Contexts reference (`needs`/`steps`) and Evaluate
    // expressions in workflows and actions (Operators).
    Ok(match folded {
        Folded::Value(_) => Folded::Residual {
            expr: expr.clone(),
            defers_on: defers,
        },
        Folded::Residual {
            expr,
            defers_on: mut folded_defers,
        } => {
            folded_defers.extend(defers);
            Folded::Residual {
                expr,
                defers_on: folded_defers,
            }
        }
    })
}

fn split_folded(f: Folded) -> (Expr, BTreeSet<DeferReason>) {
    match f {
        Folded::Value(v) => (value_to_literal_expr(&v), BTreeSet::new()),
        Folded::Residual { expr, defers_on } => (expr, defers_on),
    }
}

/// `!x`: design memo §3.2 — `Not` always produces a genuine boolean, so a
/// static operand folds all the way, while a deferred operand just carries
/// the `!` through.
fn fold_not(inner: &Expr, ctx: &FoldCtx<'_>) -> Result<Folded, PartialEvalError> {
    match fold_expr(inner, ctx)? {
        Folded::Value(v) => Ok(Folded::Value(Value::Bool(is_falsy(&v)))),
        Folded::Residual { expr, defers_on } => Ok(Folded::Residual {
            expr: Expr::Not(Box::new(expr)),
            defers_on,
        }),
    }
}

/// Produces the locally folded shape for `a && b`. [`fold_expr`] wraps a
/// locally static result back into a residual whenever the authored tree
/// contains any runtime dependency, preserving the classification
/// invariant while still simplifying safe subtrees.
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
            let (rhs_expr, rhs_defers) = split_folded(fold_expr(rhs, ctx)?);
            let mut defers = lhs_defers;
            defers.extend(rhs_defers);
            Ok(Folded::Residual {
                expr: Expr::Binary {
                    op: BinOp::And,
                    lhs: Box::new(lhs_expr),
                    rhs: Box::new(rhs_expr),
                },
                defers_on: defers,
            })
        }
    }
}

/// `a || b`: the truthy-mirror of [`fold_and`], subject to the same outer
/// runtime-dependency preservation.
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
            let (rhs_expr, rhs_defers) = split_folded(fold_expr(rhs, ctx)?);
            let mut defers = lhs_defers;
            defers.extend(rhs_defers);
            Ok(Folded::Residual {
                expr: Expr::Binary {
                    op: BinOp::Or,
                    lhs: Box::new(lhs_expr),
                    rhs: Box::new(rhs_expr),
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

/// `contains`/`startsWith`/`endsWith`/`format`/`join`/`toJSON`/`fromJSON`
/// with at least one deferred argument: reconstructs the call with each
/// argument's own fold (only ever invoked when [`fold_expr`]'s outer
/// defer-scan already found something in this subtree, so this always
/// legitimately returns a residual rather than a value).
fn fold_pure_call(
    name: &str,
    args: &[Expr],
    ctx: &FoldCtx<'_>,
) -> Result<Folded, PartialEvalError> {
    let mut arg_exprs = Vec::with_capacity(args.len());
    let mut defers = BTreeSet::new();
    for a in args {
        let (e, d) = split_folded(fold_expr(a, ctx)?);
        arg_exprs.push(e);
        defers.extend(d);
    }
    Ok(Folded::Residual {
        expr: Expr::Call {
            name: name.to_string(),
            args: arg_exprs,
        },
        defers_on: defers,
    })
}

fn is_pure_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "contains" | "startswith" | "endswith" | "format" | "join" | "tojson" | "fromjson"
    )
}

fn is_status_function(name: &str) -> Option<StatusFn> {
    match name.to_ascii_lowercase().as_str() {
        "success" => Some(StatusFn::Success),
        "failure" => Some(StatusFn::Failure),
        "cancelled" => Some(StatusFn::Cancelled),
        "always" => Some(StatusFn::Always),
        _ => None,
    }
}

/// Recursively determines every [`DeferReason`] a subtree touches, erroring
/// immediately for a forbidden `secrets.*` reference rather than merely
/// deferring it (design memo §3.1).
fn collect_defer_reasons(
    expr: &Expr,
    ctx: &FoldCtx<'_>,
    out: &mut BTreeSet<DeferReason>,
) -> Result<(), PartialEvalError> {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) => {}
        Expr::NamedValue(name) => classify_bare_root(name, ctx, out)?,
        Expr::Not(inner) => collect_defer_reasons(inner, ctx, out)?,
        Expr::Binary { lhs, rhs, .. } => {
            collect_defer_reasons(lhs, ctx, out)?;
            collect_defer_reasons(rhs, ctx, out)?;
        }
        Expr::Wildcard { target } => collect_defer_reasons(target, ctx, out)?,
        Expr::Call { name, args } => {
            if let Some(sf) = is_status_function(name) {
                out.insert(DeferReason::StatusFn(sf));
            } else if name.eq_ignore_ascii_case("hashfiles") {
                out.insert(DeferReason::HashFiles);
            }
            for a in args {
                collect_defer_reasons(a, ctx, out)?;
            }
        }
        Expr::Index { target, index } => {
            if let Some((root, segments)) = literal_path(expr) {
                classify_path(&root, &segments, ctx, out)?;
            } else {
                // An indirect `env[...]` lookup cannot be frozen from the
                // selected plan-time key: a prior step can update that
                // selected variable through GITHUB_ENV before this
                // expression runs. Literal `env['NAME']` still follows the
                // normal declared-name path above.
                if expression_root(target).is_some_and(|root| root.eq_ignore_ascii_case("env")) {
                    out.insert(DeferReason::DynamicEnv {
                        name: "*".to_string(),
                    });
                }
                collect_defer_reasons(target, ctx, out)?;
                collect_defer_reasons(index, ctx, out)?;
            }
        }
    }
    Ok(())
}

fn expression_root(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::NamedValue(name) => Some(name),
        Expr::Index { target, .. } | Expr::Wildcard { target } => expression_root(target),
        _ => None,
    }
}

fn classify_bare_root(
    name: &str,
    ctx: &FoldCtx<'_>,
    out: &mut BTreeSet<DeferReason>,
) -> Result<(), PartialEvalError> {
    if name.eq_ignore_ascii_case("secrets") {
        if ctx.secrets_forbidden {
            return Err(PartialEvalError::SecretsForbiddenInJobCondition);
        }
        out.insert(DeferReason::SecretsContext);
    } else if name.eq_ignore_ascii_case("runner") {
        out.insert(DeferReason::RunnerContext);
    } else if name.eq_ignore_ascii_case("job") {
        out.insert(DeferReason::JobContext);
    } else if name.eq_ignore_ascii_case("needs") {
        out.insert(DeferReason::NeedsContextWhole);
    } else if name.eq_ignore_ascii_case("steps") {
        out.insert(DeferReason::StepsContextWhole);
    }
    // `github`/`env`/`vars`/`matrix`/`inputs`: static roots, nothing to add.
    Ok(())
}

fn classify_path(
    root: &str,
    segments: &[String],
    ctx: &FoldCtx<'_>,
    out: &mut BTreeSet<DeferReason>,
) -> Result<(), PartialEvalError> {
    if root.eq_ignore_ascii_case("secrets") {
        if ctx.secrets_forbidden {
            return Err(PartialEvalError::SecretsForbiddenInJobCondition);
        }
        out.insert(DeferReason::SecretsContext);
    } else if root.eq_ignore_ascii_case("runner") {
        out.insert(DeferReason::RunnerContext);
    } else if root.eq_ignore_ascii_case("job") {
        out.insert(DeferReason::JobContext);
    } else if root.eq_ignore_ascii_case("needs") {
        out.insert(classify_needs_path(segments));
    } else if root.eq_ignore_ascii_case("steps") {
        out.insert(classify_steps_path(segments));
    } else if root.eq_ignore_ascii_case("env")
        && let Some(name) = segments.first()
    {
        if let Some(reasons) = ctx.env.deferred_reasons(name) {
            out.extend(reasons.iter().cloned());
        } else if !ctx.env.is_declared(name) {
            out.insert(DeferReason::DynamicEnv { name: name.clone() });
        }
    }
    // `github`/`vars`/`matrix`/`inputs`: static, nothing to add.
    Ok(())
}

fn classify_needs_path(segments: &[String]) -> DeferReason {
    let job = JobId(segments.first().cloned().unwrap_or_default());
    match segments.get(1) {
        Some(s) if s.eq_ignore_ascii_case("result") => DeferReason::NeedsResult { job },
        Some(s) if s.eq_ignore_ascii_case("outputs") => DeferReason::NeedsOutput {
            job,
            output: segments.get(2).cloned(),
        },
        Some(other) => DeferReason::NeedsOutput {
            job,
            output: Some(other.clone()),
        },
        None => DeferReason::NeedsOutput { job, output: None },
    }
}

fn classify_steps_path(segments: &[String]) -> DeferReason {
    let step = segments.first().cloned().unwrap_or_default();
    match segments.get(1) {
        Some(s) if s.eq_ignore_ascii_case("outcome") => DeferReason::StepStatus {
            step,
            field: StepStatusField::Outcome,
        },
        Some(s) if s.eq_ignore_ascii_case("conclusion") => DeferReason::StepStatus {
            step,
            field: StepStatusField::Conclusion,
        },
        Some(s) if s.eq_ignore_ascii_case("outputs") => DeferReason::StepOutput {
            step,
            output: segments.get(2).cloned(),
        },
        Some(other) => DeferReason::StepOutput {
            step,
            output: Some(other.clone()),
        },
        None => DeferReason::StepOutput { step, output: None },
    }
}

/// Descends through a chain of `Index { target, index: Str(name) }` nodes,
/// collecting literal segment names root-to-leaf, stopping at the base
/// `NamedValue`. `None` if any level's index isn't a string literal (e.g.
/// `needs[someExpr]`) or the chain doesn't bottom out in a `NamedValue`.
fn literal_path(expr: &Expr) -> Option<(String, Vec<String>)> {
    fn walk(expr: &Expr, segments: &mut Vec<String>) -> Option<String> {
        match expr {
            Expr::NamedValue(name) => Some(name.clone()),
            Expr::Index { target, index } => {
                let root = walk(target, segments)?;
                match index.as_ref() {
                    Expr::Str(s) => {
                        segments.push(s.clone());
                        Some(root)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
    let mut segments = Vec::new();
    let root = walk(expr, &mut segments)?;
    Some((root, segments))
}
