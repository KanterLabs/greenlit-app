//! Job-condition availability validation and implicit status-gate detection.

use greenlit_expr::Expr;
use greenlit_workflow::model::job::Job;

use crate::condition::{Condition, plan_condition};
use crate::graph::JobId;
use crate::partial_eval::FoldCtx;

use super::PlanError;
use super::error::eval_err;

pub(super) fn fold_job_condition(
    job: &Job,
    ctx: &FoldCtx<'_>,
    job_id: &JobId,
) -> Result<Option<Condition>, PlanError> {
    let Some(raw) = &job.if_condition else {
        return Ok(None);
    };
    validate_job_condition_availability(&raw.value, &raw.span, job_id)?;
    plan_condition(&raw.value, &raw.span, ctx)
        .map(Some)
        .map_err(|source| eval_err(job_id, None, raw.span.clone(), source))
}

/// GitHub's Context availability table is authoritative for this workflow
/// key: `jobs.<job_id>.if` may use only `github`, `needs`, `vars`, and
/// `inputs`; among restricted functions it may use only the four status
/// functions. Treating a disallowed root as an empty object would silently
/// turn an invalid GitHub workflow into a plausible local plan.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability
fn validate_job_condition_availability(
    raw: &str,
    span: &greenlit_workflow::Span,
    job_id: &JobId,
) -> Result<(), PlanError> {
    let Ok(expr) = greenlit_expr::parse(strip_wrapper(raw)) else {
        // The normal condition parser below reports the richer parse error.
        return Ok(());
    };
    let Some(unavailable) = first_unavailable_job_condition_reference(&expr) else {
        return Ok(());
    };
    Err(PlanError::JobConditionUnavailable {
        job: job_id.clone(),
        unavailable,
        span: span.clone(),
    })
}

fn first_unavailable_job_condition_reference(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) => None,
        Expr::NamedValue(name) => {
            if ["github", "needs", "vars", "inputs"]
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
            {
                None
            } else {
                Some(format!("the `{name}` context"))
            }
        }
        Expr::Not(inner) | Expr::Wildcard { target: inner } => {
            first_unavailable_job_condition_reference(inner)
        }
        Expr::Index { target, index }
        | Expr::Binary {
            lhs: target,
            rhs: index,
            ..
        } => first_unavailable_job_condition_reference(target)
            .or_else(|| first_unavailable_job_condition_reference(index)),
        Expr::Call { name, args } => {
            if name.eq_ignore_ascii_case("hashfiles") {
                return Some("the `hashFiles` function".to_string());
            }
            args.iter()
                .find_map(first_unavailable_job_condition_reference)
        }
    }
}

/// Whether GitHub's implicit `success()` gate applies, computed from the
/// *authored* condition text before folding. See GitHub's status-check docs:
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#status-check-functions>.
pub(super) fn expr_calls_status(raw: &str) -> bool {
    match greenlit_expr::parse(strip_wrapper(raw)) {
        Ok(expr) => greenlit_expr::expr_calls_status_function(&expr),
        // A parse failure here would already have surfaced from condition
        // planning, so this defensive path applies the conservative gate.
        Err(_) => false,
    }
}

fn strip_wrapper(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
        .map(str::trim)
        .unwrap_or(trimmed)
}
