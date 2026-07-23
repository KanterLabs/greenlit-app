//! Table/fake-driven tests for the step outcome model and runtime resolution
//! of deferred plan values. No container required: outcomes are computed from
//! faked exit signals and resolution runs against locally-built contexts.

use std::io;
use std::path::Path;
use std::sync::Arc;

use greenlit_engine::condition::{Condition, DeferredExpr, PlannedCond};
use greenlit_engine::execution::outcome::{
    Conclusion, StepExit, advance_status, job_result_from_status, step_activates,
    step_result_from_exit, step_result_skipped,
};
use greenlit_engine::execution::resolve::{
    resolve_bool, resolve_condition, resolve_minutes, resolve_string,
};
use greenlit_engine::planned::{Evaluation, Planned};
use greenlit_expr::{Context, EntryKind, HashFilesFs, OpenedDir, RunStatus, Value};
use greenlit_workflow::{Location, Span};

#[derive(Debug)]
struct NoFs;

impl HashFilesFs for NoFs {
    fn workspace_root(&self) -> &Path {
        Path::new("/workspace")
    }
    fn open_dir(&self, _path: &Path) -> io::Result<OpenedDir<'_>> {
        Err(io::Error::other("no fs"))
    }
    fn entry_kind(&self, _path: &Path) -> io::Result<EntryKind> {
        Err(io::Error::other("no fs"))
    }
    fn hash_file_sha256(
        &self,
        _path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        Err(io::Error::other("no fs"))
    }
}

fn span() -> Span {
    Span::new(
        Arc::from("test.yml"),
        Location::new(1, 1),
        Location::new(1, 1),
    )
}

fn ctx() -> Context {
    Context::new(Arc::new(NoFs))
}

// --- outcome model ---------------------------------------------------------

#[test]
fn step_activation_respects_implicit_success_gate() {
    // Implicit gate: runs only while the job is still succeeding.
    assert!(step_activates(RunStatus::Success, true, true));
    assert!(!step_activates(RunStatus::Failure, true, true));
    assert!(!step_activates(RunStatus::Cancelled, true, true));
    assert!(!step_activates(RunStatus::Success, true, false));
    // No implicit gate (status function present): the condition alone decides.
    assert!(step_activates(RunStatus::Failure, false, true));
    assert!(!step_activates(RunStatus::Failure, false, false));
}

#[test]
fn continue_on_error_splits_outcome_and_conclusion() {
    let ok = step_result_from_exit(StepExit::Success, false);
    assert_eq!(ok.outcome, Conclusion::Success);
    assert_eq!(ok.conclusion, Conclusion::Success);

    let failed = step_result_from_exit(StepExit::Failed, false);
    assert_eq!(failed.outcome, Conclusion::Failure);
    assert_eq!(failed.conclusion, Conclusion::Failure);

    let rescued = step_result_from_exit(StepExit::Failed, true);
    assert_eq!(rescued.outcome, Conclusion::Failure);
    assert_eq!(rescued.conclusion, Conclusion::Success);

    let timed = step_result_from_exit(StepExit::TimedOut, false);
    assert_eq!(timed.outcome, Conclusion::Failure);

    // Cancellation is never rescued by continue-on-error.
    let cancelled = step_result_from_exit(StepExit::Cancelled, true);
    assert_eq!(cancelled.outcome, Conclusion::Cancelled);
    assert_eq!(cancelled.conclusion, Conclusion::Cancelled);

    let skipped = step_result_skipped();
    assert_eq!(skipped.conclusion, Conclusion::Skipped);
}

#[test]
fn status_rollup_is_monotone_toward_severity() {
    assert_eq!(
        advance_status(RunStatus::Success, Conclusion::Success),
        RunStatus::Success
    );
    assert_eq!(
        advance_status(RunStatus::Success, Conclusion::Skipped),
        RunStatus::Success
    );
    assert_eq!(
        advance_status(RunStatus::Success, Conclusion::Failure),
        RunStatus::Failure
    );
    // A later success does not un-fail the job.
    assert_eq!(
        advance_status(RunStatus::Failure, Conclusion::Success),
        RunStatus::Failure
    );
    // Cancellation dominates and is sticky.
    assert_eq!(
        advance_status(RunStatus::Failure, Conclusion::Cancelled),
        RunStatus::Cancelled
    );
    assert_eq!(
        advance_status(RunStatus::Cancelled, Conclusion::Failure),
        RunStatus::Cancelled
    );
    assert_eq!(
        job_result_from_status(RunStatus::Failure),
        Conclusion::Failure
    );
}

// --- runtime resolution of deferred plan values ----------------------------

#[test]
fn resolves_static_and_deferred_strings() {
    let ctx = ctx().with_env(Value::object(vec![(
        "GREETING".to_string(),
        Value::String("hi".to_string()),
    )]));
    let stat = Planned::<String> {
        span: span(),
        source: "literal".to_string(),
        evaluation: Evaluation::Static("literal".to_string()),
    };
    assert_eq!(resolve_string(&stat, &ctx).unwrap(), "literal");
    let residual = greenlit_expr::parse("env.GREETING").unwrap();
    let deferred = Planned::<String> {
        span: span(),
        source: "env.GREETING".to_string(),
        evaluation: Evaluation::Deferred(DeferredExpr {
            residual,
            residual_text: "env.GREETING".to_string(),
            defers_on: Vec::new(),
        }),
    };
    assert_eq!(resolve_string(&deferred, &ctx).unwrap(), "hi");
}

#[test]
fn resolves_boolean_field_by_literal_parse_not_truthiness() {
    let ctx = ctx();
    // A deferred continue-on-error evaluating to the string "false" is false,
    // even though a non-empty string is otherwise truthy.
    let residual = greenlit_expr::parse("'false'").unwrap();
    let planned = Planned::<bool> {
        span: span(),
        source: "${{ 'false' }}".to_string(),
        evaluation: Evaluation::Deferred(DeferredExpr {
            residual,
            residual_text: "'false'".to_string(),
            defers_on: Vec::new(),
        }),
    };
    assert!(!resolve_bool(&planned, &ctx).unwrap());
}

#[test]
fn resolves_timeout_minutes_number() {
    let planned = Planned::<f64> {
        span: span(),
        source: "10".to_string(),
        evaluation: Evaluation::Static(10.0),
    };
    assert_eq!(resolve_minutes(&planned, &ctx()).unwrap(), 10.0);
}

#[test]
fn resolves_condition_with_status_function_against_live_status() {
    // `failure()` is true only when the rolling job status is failure.
    let residual = greenlit_expr::parse("failure()").unwrap();
    let cond = Condition {
        span: span(),
        source: "${{ failure() }}".to_string(),
        eval: PlannedCond::Deferred(DeferredExpr {
            residual,
            residual_text: "failure()".to_string(),
            defers_on: Vec::new(),
        }),
    };
    let failing = ctx().with_status(RunStatus::Failure);
    let succeeding = ctx().with_status(RunStatus::Success);
    assert!(resolve_condition(&cond, &failing).unwrap());
    assert!(!resolve_condition(&cond, &succeeding).unwrap());

    let stat = Condition {
        span: span(),
        source: "true".to_string(),
        eval: PlannedCond::Static(true),
    };
    assert!(resolve_condition(&stat, &succeeding).unwrap());
}
