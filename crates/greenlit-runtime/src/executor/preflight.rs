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
use greenlit_engine::{Condition, ExecutionPlan, PlannedCond};

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

#[cfg(test)]
mod tests {
    use super::*;
    use greenlit_engine::{EventKind, PlanOptions, SyntheticEvent, plan};
    use greenlit_expr::Value;

    fn planned(workflow: &str) -> ExecutionPlan {
        let parsed = greenlit_workflow::parse_workflow("preflight.yml", workflow).expect("parse");
        let event = SyntheticEvent {
            kind: EventKind::Push,
            github: Value::object(vec![(
                "event_name".to_string(),
                Value::String("push".to_string()),
            )]),
            inputs: Value::object(vec![]),
            deferred_github_properties: std::collections::BTreeSet::new(),
        };
        plan(&parsed, &event, &PlanOptions::default()).expect("plan")
    }

    #[test]
    fn a_syntactically_valid_uses_step_passes_preflight() {
        let plan = planned(
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n",
        );
        reject_uses_steps(&plan).expect("well-formed uses: passes preflight");
    }

    #[test]
    fn a_malformed_uses_reference_is_rejected() {
        let plan = planned(
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: not-a-valid-reference\n",
        );
        let error = reject_uses_steps(&plan).unwrap_err();
        assert!(matches!(error, ExecError::ActionRefInvalid { .. }));
        assert!(error.to_string().contains("preflight.yml:"));
    }

    #[test]
    fn a_statically_skipped_malformed_uses_step_stays_accepted() {
        let plan = planned(
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - if: false\n        uses: not-a-valid-reference\n      - run: echo hi\n",
        );
        reject_uses_steps(&plan).expect("an if: false uses: step never runs");
    }

    #[test]
    fn a_deferred_condition_malformed_uses_step_stays_accepted() {
        let plan = planned(
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - id: first\n        run: echo hi\n      - if: steps.first.outcome == 'success'\n        uses: not-a-valid-reference\n",
        );
        reject_uses_steps(&plan).expect("a deferred condition may resolve false at runtime");
    }
}
