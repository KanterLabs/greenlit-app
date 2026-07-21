//! Classification of expression subtrees by their runtime dependencies.

use std::collections::BTreeSet;

use greenlit_expr::Expr;

use super::{FoldCtx, PartialEvalError};
use crate::defer::{DeferReason, StatusFn, StepStatusField};
use crate::graph::JobId;

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
/// deferring it. GitHub's context-availability table excludes `secrets`
/// from `jobs.<job_id>.if`.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability>
pub(super) fn collect_defer_reasons(
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
    } else if name.eq_ignore_ascii_case("needs") && ctx.roots.needs.is_none() {
        out.insert(DeferReason::NeedsContextWhole);
    } else if name.eq_ignore_ascii_case("matrix") && ctx.roots.matrix_deferred {
        out.insert(DeferReason::MatrixContext);
    } else if name.eq_ignore_ascii_case("strategy") && ctx.roots.strategy_deferred.any() {
        out.insert(DeferReason::StrategyContext);
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
    } else if root.eq_ignore_ascii_case("needs") && ctx.roots.needs.is_none() {
        out.insert(classify_needs_path(segments));
    } else if root.eq_ignore_ascii_case("matrix") && ctx.roots.matrix_deferred {
        out.insert(DeferReason::MatrixContext);
    } else if root.eq_ignore_ascii_case("strategy") && strategy_path_is_deferred(segments, ctx) {
        out.insert(DeferReason::StrategyContext);
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

fn strategy_path_is_deferred(segments: &[String], ctx: &FoldCtx<'_>) -> bool {
    let Some(field) = segments.first() else {
        return ctx.roots.strategy_deferred.any();
    };
    ctx.roots.strategy_deferred.field(field)
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
