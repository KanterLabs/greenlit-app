//! The executor: drive a resolved [`ExecutionPlan`] to completion against a
//! [`ContainerEngine`], one fresh container per job and one exec per step.
//!
//! `PHASE-2-execution.md` ("Job and step execution semantics", "Output and
//! metrics"): one container per job; each step is an exec in that container;
//! ready jobs and matrix legs run concurrently while steps remain sequential.
//! Every
//! GitHub-faithful rule — shell resolution, env layering, command files, the
//! `outcome`/`conclusion` model, `needs` propagation, job outputs — is the
//! engine's [`greenlit_engine::execution`] semantics; this module is the
//! container-side driver that runs them.
//!
//! The executor streams live logs (group folding, per-step status lines, secret
//! masking) to the supplied writer while it works, and returns a data-only
//! [`RunReport`] for the caller to render the end-of-run table and record
//! metrics from. Every new pipeline stage opens a timed `tracing` span
//! (`greenlit_metrics::timed_stage`) the day it is built, so `greenlit-metrics`
//! captures the stage breakdown without a dependency edge back onto this crate.

pub mod actions;
mod cmdfiles;
pub mod container;
mod context;
mod dind;
mod event_json;
mod execution_api;
mod health;
mod image_lock;
mod instance;
mod job;
mod logsink;
mod netguard;
mod preflight;
mod quarantine;
mod readiness;
mod report;
mod runner_lock;
mod runner_profile;
mod scheduler;
mod services;
mod step;
mod step_ids;
mod worker_pool;

use std::io::Write;
use std::sync::Arc;

use indexmap::IndexMap;

use greenlit_engine::execution::Masker;
use greenlit_engine::execution::contexts::merge_matrix_outputs;
use greenlit_engine::{Conclusion, EnvValue, ExecutionPlan};
use greenlit_expr::RealFs;

use crate::executor::context::ContextRoots;
use crate::progress::ProgressSink;

pub use actions::ActionPreflight;
pub use container::{
    ContainerAdditions, ContainerRejection as JobContainerRejection, ResolvedContainer,
};
pub use execution_api::{ExecError, RunConfig};
pub use image_lock::preflight_plan_images;
pub use preflight::{reject_hermetic_late_inputs, reject_uses_steps};
pub use quarantine::{
    PlanReachability, RuntimeAuthorization, RuntimeCapabilityAssessment, RuntimeCapabilityInputs,
    RuntimeControl, assess_runtime_capabilities, plan_reachability,
};
pub use readiness::ReadinessConfig;
pub use report::{JobReport, RunReport, StepReport};
pub use runner_lock::preflight_plan_runners;
pub use services::{JobNetwork, StoreConfig};

/// A completed job's aggregate result and its merged outputs, for dependents.
#[derive(Clone)]
pub(crate) struct CompletedJob {
    pub(crate) result: Conclusion,
    pub(crate) outputs: IndexMap<String, String>,
    /// Whether this job or any job in its own ancestor chain failed
    /// (transitively) — GitHub: "If you have a chain of dependent jobs,
    /// failure() returns true if any ancestor job fails."
    /// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#status-check-functions>
    pub(crate) chain_failed: bool,
    /// Whether this job or any ancestor was cancelled (transitively), mirroring
    /// `chain_failed`'s propagation for `cancelled()`.
    pub(crate) chain_cancelled: bool,
}

/// The run-wide, immutable context every job instance shares.
pub(crate) struct Shared<'a> {
    /// The container engine.
    pub engine: &'a dyn ContainerEngine,
    /// The run configuration.
    pub config: &'a RunConfig,
    /// The run-wide expression context roots.
    pub roots: &'a ContextRoots,
    /// The workflow-level `env:` plan (resolved per job).
    pub workflow_env: &'a IndexMap<String, EnvValue>,
    /// Cooperative cancellation for this invocation.
    pub cancellation: &'a crate::Cancellation,
    /// Resource namespace for this run or concrete job instance.
    pub namespace: &'a str,
}

/// Open a timed stage span captured by `greenlit-metrics`'s timing layer.
pub(crate) fn stage_span(name: &'static str) -> tracing::Span {
    tracing::info_span!(
        target: "greenlit_metrics::timed_stage",
        "greenlit_stage",
        stage = name
    )
}

/// Execute `plan` against `engine`, streaming live logs to `out` and returning
/// the structured [`RunReport`].
///
/// # Errors
///
/// Returns an [`ExecError`] on any engine failure, containment-breaking job
/// container, unfinished expression, unsupported shell, `uses:` step, or
/// runtime-deferred matrix/runner. Ordinary step *failures* are not errors —
/// they are recorded in the report and reflected in [`RunReport::overall`].
pub async fn run_plan(
    engine: &dyn ContainerEngine,
    plan: &ExecutionPlan,
    config: &RunConfig,
    authorization: RuntimeAuthorization,
    out: &mut (dyn Write + Send),
    progress: &mut (dyn ProgressSink + Send),
) -> Result<RunReport, ExecError> {
    run_plan_cancellable(
        engine,
        plan,
        config,
        authorization,
        out,
        progress,
        &crate::Cancellation::new(),
    )
    .await
}

/// Execute a plan with a cooperative cancellation signal.
///
/// Cancellation stops queued work and tears down active job containers,
/// services, sidecars, volumes, and networks before returning a cancelled
/// report.
pub async fn run_plan_cancellable(
    engine: &dyn ContainerEngine,
    plan: &ExecutionPlan,
    config: &RunConfig,
    authorization: RuntimeAuthorization,
    out: &mut (dyn Write + Send),
    progress: &mut (dyn ProgressSink + Send),
    cancellation: &crate::Cancellation,
) -> Result<RunReport, ExecError> {
    let mut logs = crate::events::FlatLogSink::new(out);
    let mut events = crate::events::ExecutionEventNull;
    run_plan_with_events_cancellable(
        engine,
        plan,
        config,
        RuntimeControl::new(authorization, cancellation),
        &mut logs,
        &mut events,
        progress,
    )
    .await
}

/// Execute a plan with job-scoped logs and structured lifecycle events.
///
/// This is the presentation-neutral execution entrypoint used by the CLI's
/// plain, JSONL, persisted-log, and future interactive renderers.
///
/// # Errors
///
/// Returns the same execution and infrastructure errors as
/// [`run_plan_cancellable`].
pub async fn run_plan_with_events_cancellable(
    engine: &dyn ContainerEngine,
    plan: &ExecutionPlan,
    config: &RunConfig,
    control: RuntimeControl<'_>,
    logs: &mut (dyn crate::events::RunLogSink + Send),
    events: &mut (dyn crate::events::ExecutionEventSink + Send),
    progress: &mut (dyn ProgressSink + Send),
) -> Result<RunReport, ExecError> {
    quarantine::enforce_runtime_quarantine(
        plan,
        config,
        control.authorization,
        control.assessment,
    )?;
    let cancellation = control.cancellation;

    let mut masker = Masker::new();
    for value in &config.initial_masks {
        masker.add(value);
    }

    let groups = instance::expand(plan)?;
    let fs = Arc::new(RealFs::new(config.repo_host_path.clone()));
    let roots = ContextRoots {
        github: config.github.clone(),
        vars: config.vars.clone(),
        inputs: config.inputs.clone(),
        secrets: config.secrets.clone(),
        fs,
    };
    let shared = Shared {
        engine,
        config,
        roots: &roots,
        workflow_env: &plan.env,
        cancellation,
        namespace: &config.volume_namespace,
    };

    scheduler::run(&shared, &groups, &masker, logs, events, progress).await
}

/// Aggregate a job's per-leg results into the single result and merged output
/// map its dependents observe.
///
/// GitHub reports a matrix job's `needs.<id>.result` as the worst leg outcome
/// (a single failed leg fails the dependency) and merges leg outputs, the last
/// writer winning per key (`greenlit_engine::execution::contexts`).
pub(crate) fn aggregate(
    results: &[(Conclusion, IndexMap<String, String>)],
) -> (Conclusion, IndexMap<String, String>) {
    let result = if results.is_empty() {
        // A zero-leg matrix produced no instance; dependents see it as skipped.
        Conclusion::Skipped
    } else if results
        .iter()
        .any(|(conclusion, _)| matches!(conclusion, Conclusion::Failure))
    {
        Conclusion::Failure
    } else if results
        .iter()
        .any(|(conclusion, _)| matches!(conclusion, Conclusion::Cancelled))
    {
        Conclusion::Cancelled
    } else if results
        .iter()
        .all(|(conclusion, _)| matches!(conclusion, Conclusion::Skipped))
    {
        Conclusion::Skipped
    } else {
        Conclusion::Success
    };
    let outputs = merge_matrix_outputs(results.iter().map(|(_, outputs)| outputs));
    (result, outputs)
}

pub use crate::engine::ContainerEngine;
