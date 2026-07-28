//! Pre-boot rejection of plans `litci run` cannot execute at all.
//!
//! Phase 2 rejected *every* `uses:` step here, before any engine work,
//! because no execution path existed yet. Phase 3 adds one: a `uses:` step
//! now actually resolves, fetches, and runs
//! (`crate::executor::actions`) — so the only thing left to reject this
//! early is a `uses:` value that cannot even be *parsed* into one of
//! GitHub's four documented forms (`owner/repo@ref`, `owner/repo/subdir@ref`,
//! `./local/path`, `docker://image`). Everything parseable proceeds to
//! resolution and execution, whose own failures (a ref that does not
//! resolve, a fetch that fails, an unsupported nested construct) surface at
//! their natural point in the run rather than being front-loaded here —
//! most of them need network/store access this pre-boot scan deliberately
//! does not perform, so front-loading them would either duplicate that work
//! or silently skip checking jobs this scan does not run for.
//!
//! Semantics-preserving by design: a malformed `uses:` behind `if: false`
//! (or a statically skipped job/leg) never runs today and stays accepted,
//! and a deferred condition is left to the exec-time guard — it may legally
//! resolve false at runtime, keeping the run green.

use greenlit_actions::ActionRef;
use greenlit_engine::plan::{StepKind, StepPlan};
use greenlit_engine::{Condition, Evaluation, ExecutionPlan, PlannedCond};

use crate::executor::ExecError;

/// Rejects the first `uses:` step in `plan` whose reference is malformed and
/// certain to execute.
///
/// # Errors
///
/// Returns [`ExecError::ActionRefInvalid`] for the first offending step in
/// file order, with its authored span.
pub fn reject_uses_steps(plan: &ExecutionPlan) -> Result<(), ExecError> {
    for job in &plan.jobs {
        if job.skip.is_some() {
            continue;
        }
        // A non-matrix job (or deferred-matrix template) carries its own
        // steps; a statically expanded matrix carries per-leg step lists.
        scan_steps(&job.steps)?;
        for leg in &job.legs {
            if leg.skip.is_some() {
                continue;
            }
            scan_steps(&leg.steps)?;
        }
    }
    Ok(())
}

/// Rejects checkout inputs that would make hermetic execution discover source
/// identity after the immutable run lock has been finalized.
///
/// GitHub's checkout action defaults to the current repository. Greenlit
/// satisfies that case from the frozen source snapshot. A different
/// repository performs a network fetch, while a deferred repository input is
/// not knowable at lock time; both are late mutable inputs and therefore fail
/// closed in hermetic mode.
///
/// # Errors
///
/// Returns [`ExecError::HermeticLateInput`] for the first affected checkout.
pub fn reject_hermetic_late_inputs(
    plan: &ExecutionPlan,
    current_repository: &str,
) -> Result<(), ExecError> {
    for job in &plan.jobs {
        if job.skip.is_some() {
            continue;
        }
        scan_hermetic_steps(&job.steps, current_repository)?;
        for leg in &job.legs {
            if leg.skip.is_some() {
                continue;
            }
            scan_hermetic_steps(&leg.steps, current_repository)?;
        }
    }
    Ok(())
}

fn scan_hermetic_steps(steps: &[StepPlan], current_repository: &str) -> Result<(), ExecError> {
    for step in steps {
        let StepKind::Uses {
            reference,
            span,
            with,
        } = &step.kind
        else {
            continue;
        };
        let Ok(ActionRef::Repository(remote)) = ActionRef::parse(reference) else {
            continue;
        };
        if remote.owner != "actions" || remote.repo != "checkout" {
            continue;
        }
        let Some(repository) = with.get("repository") else {
            continue;
        };
        match &repository.evaluation {
            Evaluation::Static(value) if value.is_empty() || value == current_repository => {}
            Evaluation::Static(value) => {
                return Err(ExecError::HermeticLateInput {
                    input: format!("repository={value}"),
                    span: span.clone(),
                });
            }
            Evaluation::Deferred(_) => {
                return Err(ExecError::HermeticLateInput {
                    input: format!("repository={}", repository.source),
                    span: span.clone(),
                });
            }
        }
    }
    Ok(())
}

fn scan_steps(steps: &[StepPlan]) -> Result<(), ExecError> {
    for step in steps {
        let StepKind::Uses {
            reference, span, ..
        } = &step.kind
        else {
            continue;
        };
        if !certain_to_run(step.condition.as_ref()) {
            continue;
        }
        if let Err(source) = ActionRef::parse(reference) {
            return Err(ExecError::ActionRefInvalid {
                reference: reference.clone(),
                span: span.clone(),
                source,
            });
        }
    }
    Ok(())
}

/// Whether a step with this `if:` planning result is certain to execute.
/// `Static(false)` is a plan-time skip; `Deferred` may still resolve false
/// at runtime, so it is not certain.
fn certain_to_run(condition: Option<&Condition>) -> bool {
    match condition {
        None => true,
        Some(condition) => matches!(condition.eval, PlannedCond::Static(true)),
    }
}
