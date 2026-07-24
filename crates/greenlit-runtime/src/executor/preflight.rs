//! Pre-boot rejection of plans `litci run` cannot execute yet.
//!
//! A `uses:` step fails at execution time ([`super::step`]'s guard), but by
//! then an image has been ensured and a container booted — worst case the
//! user waits through a long workspace copy just to learn the workflow was
//! never runnable. This scan front-loads that answer: it walks the pruned
//! plan before any engine work and rejects the first `uses:` step that will
//! definitely execute.
//!
//! Semantics-preserving by design: a `uses:` step behind `if: false` (or a
//! statically skipped job/leg) never runs today and stays accepted, and a
//! deferred condition is left to the exec-time guard — it may legally
//! resolve false at runtime, keeping the run green.

use greenlit_engine::plan::{StepKind, StepPlan};
use greenlit_engine::{Condition, ExecutionPlan, PlannedCond};

use crate::executor::ExecError;

/// Rejects the first `uses:` step in `plan` that is certain to execute.
///
/// # Errors
///
/// Returns [`ExecError::UsesUnsupported`] for the first offending step in
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

fn scan_steps(steps: &[StepPlan]) -> Result<(), ExecError> {
    for step in steps {
        let StepKind::Uses {
            reference, span, ..
        } = &step.kind
        else {
            continue;
        };
        if certain_to_run(step.condition.as_ref()) {
            return Err(ExecError::UsesUnsupported {
                reference: reference.clone(),
                span: span.clone(),
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
