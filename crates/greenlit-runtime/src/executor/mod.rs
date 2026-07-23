//! The executor: drive a resolved [`ExecutionPlan`] to completion against a
//! [`ContainerEngine`], one container per job, one exec per step, sequentially
//! in DAG order.
//!
//! `PHASE-2-execution.md` ("Job and step execution semantics", "Output and
//! metrics"): one container per job; each step is an exec in that container;
//! jobs run sequentially in topological order (parallelism is Phase 5). Every
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

mod cmdfiles;
pub mod container;
mod context;
mod instance;
mod job;
mod logsink;
mod report;
mod step;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;

use greenlit_engine::execution::contexts::merge_matrix_outputs;
use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::execution::job_outputs::JobOutputError;
use greenlit_engine::execution::{Masker, NeedRecord};
use greenlit_engine::{Conclusion, EnvValue, ExecutionPlan, JobId};
use greenlit_expr::{EvalError, RealFs, Value};
use greenlit_workflow::Span;

use crate::error::RuntimeError;
use crate::executor::container::ContainerRejection;
use crate::executor::context::ContextRoots;
use crate::image::ImageError;
use crate::isolation::IsolationStrategy;

pub use container::{
    ContainerAdditions, ContainerRejection as JobContainerRejection, ResolvedContainer,
};
pub use report::{JobReport, RunReport, StepReport};

/// Everything the executor needs beyond the plan and the engine.
pub struct RunConfig {
    /// Absolute host path of the repository checkout (the read-only lower).
    pub repo_host_path: PathBuf,
    /// `GITHUB_WORKSPACE` inside every job container.
    pub workspace: String,
    /// Which isolation mechanism `greenlit-init` should use.
    pub strategy: IsolationStrategy,
    /// The runner/`github` env template; the executor sets `job`/`workspace`
    /// per instance.
    pub runner_env: RunnerEnv,
    /// The `github` context (from the synthetic event).
    pub github: Value,
    /// The resolved `vars` context.
    pub vars: Value,
    /// The `inputs` context (`workflow_dispatch`, else empty).
    pub inputs: Value,
    /// The `secrets` context (empty until Phase 3).
    pub secrets: Value,
    /// Values to mask from the first line of output (from `::add-mask::`-style
    /// pre-registration; secret-context masking arrives in Phase 3).
    pub initial_masks: Vec<String>,
    /// A token unique to this `litci run` invocation, used to namespace any
    /// `jobs.<id>.container.volumes:` named-volume source
    /// (`crate::executor::container::validate_container`) so a workflow can
    /// never target a pre-existing daemon-global named volume by name —
    /// GitHub's own hosted runner gives the same guarantee for free (a fresh
    /// VM per run has no pre-existing volumes to collide with); the local
    /// daemon persists across runs, so Greenlit must manufacture the
    /// equivalent isolation. Every job/leg within one run shares this token,
    /// so two job containers in the same run that both name `cache:/data`
    /// still share one (run-scoped) volume, matching the "reused across
    /// containers within one VM" behavior a workflow author would observe on
    /// GitHub.
    pub volume_namespace: String,
    /// Whether `--write-back` was requested. When `true`, a ran job's
    /// container is kept alive (not torn down) so its overlay upper can be
    /// exported after the whole run finishes (`JobReport::container_id`);
    /// the caller is responsible for removing it once write-back has run.
    pub write_back: bool,
}

/// A failure during execution. Detection-time engine conditions never travel
/// here — they are [`crate::EngineState`] variants with their own fix actions.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// A container-engine operation failed after the daemon was reached.
    #[error(transparent)]
    Engine(#[from] RuntimeError),
    /// Ensuring or building the base image failed.
    #[error(transparent)]
    Image(#[from] ImageError),
    /// A job container request was containment-breaking or unsupported.
    #[error(transparent)]
    Container(#[from] ContainerRejection),
    /// A step's command file (`GITHUB_ENV`/`GITHUB_OUTPUT`/`GITHUB_PATH`, or
    /// preparing the step's script) was malformed or could not be
    /// materialized.
    ///
    /// The message is masked (`Masker::apply`) at the point this variant is
    /// built — *before* it is wrapped as a `Display`-able error — because a
    /// malformed `GITHUB_ENV`/`GITHUB_OUTPUT` line embeds the offending line
    /// verbatim (`CommandFileError::InvalidLine`), and that line can itself be
    /// (or contain) a value an earlier `::add-mask::` registered. Storing the
    /// already-redacted `String` rather than the original typed error means
    /// every downstream consumer of this error's `Display` (the live log,
    /// `anyhow` chains, `litci`'s top-level stderr writer) sees only redacted
    /// text, satisfying "secret values are masked in all log output"
    /// (`AGENTS.md`) even for a failure path, not only successful output.
    #[error("{0}")]
    CommandFile(String),
    /// Finalizing a job's outputs failed.
    #[error(transparent)]
    JobOutput(#[from] JobOutputError),
    /// A plan-time-deferred expression could not be finished at runtime.
    #[error("could not finish evaluating an expression at runtime: {0}")]
    Eval(#[source] EvalError),
    /// A step's `shell:` could not be resolved.
    #[error("step '{label}': {source}")]
    Shell {
        /// The step's display label.
        label: String,
        /// The underlying shell-resolution error.
        #[source]
        source: greenlit_engine::execution::shell::ShellError,
    },
    /// A step's `timeout-minutes` — possibly resolved from a deferred
    /// expression — failed to evaluate or resolved outside GitHub's
    /// supported range.
    #[error("step '{label}': {source}")]
    Timeout {
        /// The step's display label.
        label: String,
        /// The underlying resolution error.
        #[source]
        source: greenlit_engine::execution::resolve::TimeoutMinutesError,
    },
    /// A `uses:` step was reached — action execution lands in Phase 3.
    #[error(
        "`uses: {reference}` is an action step, which `litci run` executes starting in Phase 3\n  fix: for now run a shell-only workflow, or use `litci plan` to inspect this one"
    )]
    UsesUnsupported {
        /// The action reference.
        reference: String,
    },
    /// A matrix that only materializes from runtime dependency outputs was
    /// reached. Expanding it requires mid-run data v0's sequential executor
    /// does not model.
    #[error(
        "{span}: this job's matrix expands from another job's outputs, which `litci run` does not execute in v0\n  fix: use a statically-defined `strategy.matrix`, or inspect the plan with `litci plan`"
    )]
    DeferredMatrix {
        /// Where the matrix was authored.
        span: Span,
    },
    /// A `runs-on:` label that only resolves from runtime data was reached.
    #[error(
        "{span}: this job's `runs-on:` label depends on runtime data, which `litci run` does not resolve in v0\n  fix: use a literal `runs-on:` label or `${{{{ matrix.* }}}}`"
    )]
    DeferredRunner {
        /// Where `runs-on:` was authored.
        span: Span,
    },
    /// A container-side setup step (helper staging, readiness) failed in a way
    /// that is neither a daemon error nor a step failure.
    #[error("{message}\n  fix: {fix}")]
    Infrastructure {
        /// What went wrong.
        message: String,
        /// The one action that resolves it.
        fix: String,
    },
}

impl ExecError {
    /// Wrap a runtime expression-evaluation failure.
    pub(crate) fn eval(source: EvalError) -> Self {
        ExecError::Eval(source)
    }
}

/// A completed job's aggregate result and its merged outputs, for dependents.
struct CompletedJob {
    result: Conclusion,
    outputs: IndexMap<String, String>,
    /// Whether this job or any job in its own ancestor chain failed
    /// (transitively) — GitHub: "If you have a chain of dependent jobs,
    /// failure() returns true if any ancestor job fails."
    /// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#status-check-functions>
    chain_failed: bool,
    /// Whether this job or any ancestor was cancelled (transitively), mirroring
    /// `chain_failed`'s propagation for `cancelled()`.
    chain_cancelled: bool,
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
    out: &mut (dyn Write + Send),
) -> Result<RunReport, ExecError> {
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
    };

    let mut job_reports: Vec<JobReport> = Vec::new();
    let mut completed: HashMap<String, CompletedJob> = HashMap::new();

    for group in &groups {
        let mut instance_results: Vec<(Conclusion, IndexMap<String, String>)> = Vec::new();
        for instance in &group.instances {
            let needs = needs_records(instance.needs, &completed);
            let report =
                job::run_instance(&shared, &mut masker, instance, &group.id, &needs, out).await?;
            instance_results.push((report.result, report.outputs.clone()));
            job_reports.push(report);
        }
        // The group's own ancestor-chain flags combine its own aggregate
        // result with every direct dependency's already-computed chain
        // flags, so a failure (or cancellation) keeps propagating downstream
        // even through an intermediate job that itself only *skipped*
        // because of it (finding: transitive `failure()` across ancestors).
        let aggregated = aggregate(&instance_results);
        let ancestors_failed = group
            .needs
            .iter()
            .any(|need| completed.get(&need.0).is_some_and(|done| done.chain_failed));
        let ancestors_cancelled = group.needs.iter().any(|need| {
            completed
                .get(&need.0)
                .is_some_and(|done| done.chain_cancelled)
        });
        let chain_failed = ancestors_failed || matches!(aggregated.0, Conclusion::Failure);
        let chain_cancelled = ancestors_cancelled || matches!(aggregated.0, Conclusion::Cancelled);
        completed.insert(
            group.id.0.clone(),
            CompletedJob {
                result: aggregated.0,
                outputs: aggregated.1,
                chain_failed,
                chain_cancelled,
            },
        );
    }

    let overall = RunReport::overall_of(&job_reports);
    Ok(RunReport {
        jobs: job_reports,
        overall,
    })
}

/// Build the `needs` records for one instance from the completed dependencies.
fn needs_records(needs: &[JobId], completed: &HashMap<String, CompletedJob>) -> Vec<NeedRecord> {
    needs
        .iter()
        .filter_map(|need| {
            completed.get(&need.0).map(|done| NeedRecord {
                job: need.clone(),
                result: done.result,
                outputs: done.outputs.clone(),
                chain_failed: done.chain_failed,
                chain_cancelled: done.chain_cancelled,
            })
        })
        .collect()
}

/// Aggregate a job's per-leg results into the single result and merged output
/// map its dependents observe.
///
/// GitHub reports a matrix job's `needs.<id>.result` as the worst leg outcome
/// (a single failed leg fails the dependency) and merges leg outputs, the last
/// writer winning per key (`greenlit_engine::execution::contexts`).
fn aggregate(
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
