//! Classification of expression subtrees by their runtime dependencies.

use std::collections::BTreeSet;

use greenlit_expr::{Expr, Value};

use super::{FoldCtx, Folded, NeedsContextSlots, PartialEvalError, StepsContextSlots, fold_expr};
use crate::defer::{DeferReason, StatusFn, StepStatusField};

mod paths;

use paths::{expression_root, static_path};

/// Properties GitHub documents on the `github` context but which a local
/// Phase 1 synthetic event cannot know. Step/action paths and identities are
/// runner-created; run identity and retention are assigned by GitHub; IDs,
/// branch protection, and repository URL are API-backed; token and secret
/// source depend on execution credentials.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#github-context>
const DEFERRED_GITHUB_PROPERTIES: &[&str] = &[
    "action",
    "action_path",
    "action_ref",
    "action_repository",
    "action_status",
    "actor_id",
    "env",
    "event_path",
    "job",
    "path",
    "ref_protected",
    "repository_id",
    "repository_owner_id",
    "repositoryUrl",
    "retention_days",
    "run_attempt",
    "run_id",
    "run_number",
    "secret_source",
    "token",
    "workspace",
];

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
            if let Some((root, segments)) = static_path(expr, ctx)? {
                classify_path(&root, &segments, ctx, out)?;
            } else {
                // A genuinely runtime-dependent or non-scalar `env[...]`
                // key cannot identify one variable. Fully static scalar
                // indexes take the exact-name path above, including when a
                // prior step makes that one name runtime-mutable.
                if expression_root(target).is_some_and(|root| root.eq_ignore_ascii_case("env")) {
                    out.insert(DeferReason::DynamicEnv {
                        name: "*".to_string(),
                    });
                }
                match fold_expr(target, ctx)? {
                    Folded::Value(
                        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_),
                    ) => {
                        // `Index.cs` returns before evaluating the key for
                        // a primitive target, so it adds no dependency.
                    }
                    Folded::Value(Value::Array(_) | Value::Object(_)) => {
                        collect_defer_reasons(index, ctx, out)?;
                    }
                    Folded::Residual { defers_on, .. } => {
                        out.extend(defers_on);
                        collect_defer_reasons(index, ctx, out)?;
                    }
                }
            }
        }
    }
    Ok(())
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
    } else if name.eq_ignore_ascii_case("github") {
        out.insert(DeferReason::GithubContext { property: None });
    } else if name.eq_ignore_ascii_case("runner") {
        out.insert(DeferReason::RunnerContext);
    } else if name.eq_ignore_ascii_case("job") {
        out.insert(DeferReason::JobContext);
    } else if name.eq_ignore_ascii_case("needs") && !ctx.roots.needs_slots.is_empty() {
        out.insert(DeferReason::NeedsContextWhole);
    } else if name.eq_ignore_ascii_case("matrix") && ctx.roots.matrix_deferred {
        out.insert(DeferReason::MatrixContext);
    } else if name.eq_ignore_ascii_case("strategy") && ctx.roots.strategy_deferred.any() {
        out.insert(DeferReason::StrategyContext);
    } else if name.eq_ignore_ascii_case("steps") && !ctx.roots.steps_slots.is_empty() {
        out.insert(DeferReason::StepsContextWhole);
    }
    // `env`/`vars`/`matrix`/`inputs`: static roots, nothing to add.
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
    } else if root.eq_ignore_ascii_case("github") {
        if let Some(property) = segments.first().and_then(|name| {
            DEFERRED_GITHUB_PROPERTIES
                .iter()
                .copied()
                .find(|property| property.eq_ignore_ascii_case(name))
                .or_else(|| {
                    ctx.roots
                        .github_deferred
                        .iter()
                        .find(|property| property.eq_ignore_ascii_case(name))
                        .map(String::as_str)
                })
        }) {
            out.insert(DeferReason::GithubContext {
                property: Some(property.to_string()),
            });
        }
    } else if root.eq_ignore_ascii_case("runner") {
        out.insert(DeferReason::RunnerContext);
    } else if root.eq_ignore_ascii_case("job") {
        out.insert(DeferReason::JobContext);
    } else if root.eq_ignore_ascii_case("needs") {
        classify_needs_path(segments, ctx.roots.needs_slots, out);
    } else if root.eq_ignore_ascii_case("matrix") && ctx.roots.matrix_deferred {
        out.insert(DeferReason::MatrixContext);
    } else if root.eq_ignore_ascii_case("strategy") && strategy_path_is_deferred(segments, ctx) {
        out.insert(DeferReason::StrategyContext);
    } else if root.eq_ignore_ascii_case("steps") {
        classify_steps_path(segments, ctx.roots.steps_slots, out);
    } else if root.eq_ignore_ascii_case("env")
        && let Some(name) = segments.first()
    {
        if let Some(reasons) = ctx.env.deferred_reasons(name) {
            out.extend(reasons.iter().cloned());
        }
        if ctx.env.is_runtime_mutable(name) {
            out.insert(DeferReason::DynamicEnv { name: name.clone() });
        }
    }
    // Synthetic event-backed `github` properties and all
    // `vars`/`matrix`/`inputs` properties are static here.
    Ok(())
}

fn strategy_path_is_deferred(segments: &[String], ctx: &FoldCtx<'_>) -> bool {
    let Some(field) = segments.first() else {
        return ctx.roots.strategy_deferred.any();
    };
    ctx.roots.strategy_deferred.field(field)
}

fn classify_needs_path(
    segments: &[String],
    slots: &NeedsContextSlots,
    out: &mut BTreeSet<DeferReason>,
) {
    let Some(referenced_job) = segments.first() else {
        return;
    };
    let Some(job) = slots.canonical_job_id(referenced_job).cloned() else {
        return;
    };
    match segments.get(1) {
        Some(field) if field.eq_ignore_ascii_case("result") => {
            out.insert(DeferReason::NeedsResult { job });
        }
        Some(field) if field.eq_ignore_ascii_case("outputs") => match segments.get(2) {
            Some(output) if slots.declares_output(referenced_job, output) => {
                out.insert(DeferReason::NeedsOutput {
                    job,
                    output: Some(output.clone()),
                });
            }
            None if slots.has_declared_outputs(referenced_job) => {
                out.insert(DeferReason::NeedsOutput { job, output: None });
            }
            Some(_) | None => {}
        },
        None => {
            out.insert(DeferReason::NeedsResult { job: job.clone() });
            if slots.has_declared_outputs(referenced_job) {
                out.insert(DeferReason::NeedsOutput { job, output: None });
            }
        }
        Some(_) => {}
    }
}

fn classify_steps_path(
    segments: &[String],
    slots: &StepsContextSlots,
    out: &mut BTreeSet<DeferReason>,
) {
    let Some(step) = segments
        .first()
        .filter(|step| slots.contains(step))
        .cloned()
    else {
        return;
    };
    match segments.get(1) {
        Some(field) if field.eq_ignore_ascii_case("outcome") => {
            out.insert(DeferReason::StepStatus {
                step,
                field: StepStatusField::Outcome,
            });
        }
        Some(field) if field.eq_ignore_ascii_case("conclusion") => {
            out.insert(DeferReason::StepStatus {
                step,
                field: StepStatusField::Conclusion,
            });
        }
        Some(field) if field.eq_ignore_ascii_case("outputs") => {
            out.insert(DeferReason::StepOutput {
                step,
                output: segments.get(2).cloned(),
            });
        }
        None => {
            out.insert(DeferReason::StepOutput {
                step: step.clone(),
                output: None,
            });
            out.insert(DeferReason::StepStatus {
                step: step.clone(),
                field: StepStatusField::Outcome,
            });
            out.insert(DeferReason::StepStatus {
                step,
                field: StepStatusField::Conclusion,
            });
        }
        Some(_) => {}
    }
}
