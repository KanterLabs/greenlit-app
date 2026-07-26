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
mod health;
mod image_lock;
mod instance;
mod job;
mod logsink;
mod netguard;
mod preflight;
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
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;

use greenlit_engine::execution::Masker;
use greenlit_engine::execution::contexts::merge_matrix_outputs;
use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::execution::job_outputs::JobOutputError;
use greenlit_engine::{Conclusion, EnvValue, ExecutionPlan};
use greenlit_expr::{EvalError, RealFs, Value};
use greenlit_workflow::Span;

use crate::error::RuntimeError;
use crate::executor::container::ContainerRejection;
use crate::executor::context::ContextRoots;
use crate::image::ImageError;
use crate::isolation::IsolationStrategy;
use crate::progress::ProgressSink;

pub use actions::ActionPreflight;
pub use container::{
    ContainerAdditions, ContainerRejection as JobContainerRejection, ResolvedContainer,
};
pub use image_lock::preflight_plan_images;
pub use preflight::{reject_hermetic_late_inputs, reject_uses_steps};
pub use readiness::ReadinessConfig;
pub use report::{JobReport, RunReport, StepReport};
pub use runner_lock::preflight_plan_runners;
pub use services::{JobNetwork, StoreConfig};

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
    /// equivalent isolation. The executor appends a concrete job/leg key
    /// before creating writable resources, so they cannot cross the fresh-job
    /// boundary.
    pub volume_namespace: String,
    /// Requested container aliases and reserved per-job runner keys mapped to
    /// the immutable image identities finalized in the RunLock. `None` is
    /// reserved for injected test executors that do not perform host-side
    /// resolution.
    pub locked_images: Option<std::collections::BTreeMap<String, String>>,
    /// Whether `--write-back` was requested. When `true`, a ran job's
    /// container is kept alive (not torn down) so its overlay upper can be
    /// exported after the whole run finishes (`JobReport::container_id`);
    /// the caller is responsible for removing it once write-back has run.
    pub write_back: bool,
    /// Cadence and deadlines for the workspace-readiness poll.
    pub readiness: ReadinessConfig,
    /// Action resolution/fetch/runtime configuration for `uses:` steps
    /// (`PHASE-3-actions.md` "Action execution").
    pub actions: actions::ActionRuntimeConfig,
    /// Where the local cache, artifact, and toolcache stores live, when this
    /// run serves them. `None` runs with no cache service at all.
    pub store: Option<StoreConfig>,
    /// Host-enforced ceilings applied to every job and service container.
    pub resources: crate::ResourceLimits,
}

impl RunConfig {
    fn locked_image(&self, requested: &str) -> Result<String, ExecError> {
        resolve_locked_image(self.locked_images.as_ref(), requested)
    }

    fn locked_runner(
        &self,
        job: &str,
        matrix_index: usize,
        fallback: &str,
    ) -> Result<String, ExecError> {
        let Some(locks) = self.locked_images.as_ref() else {
            return Ok(fallback.to_string());
        };
        const PREFIX: &str = "__greenlit_runner:";
        if !locks.keys().any(|key| key.starts_with(PREFIX)) {
            return Ok(fallback.to_string());
        }
        let matrix_key = format!("{PREFIX}{job}[{matrix_index}]");
        let job_key = format!("{PREFIX}{job}");
        locks
            .get(&matrix_key)
            .or_else(|| locks.get(&job_key))
            .cloned()
            .ok_or_else(|| ExecError::Infrastructure {
                message: format!("runner for job '{job}' is absent from the finalized RunLock"),
                fix: "preserve the run directory and file a Greenlit defect".to_string(),
            })
    }
}

fn resolve_locked_image(
    locks: Option<&std::collections::BTreeMap<String, String>>,
    requested: &str,
) -> Result<String, ExecError> {
    let Some(locks) = locks else {
        return Ok(requested.to_string());
    };
    locks
        .get(requested)
        .cloned()
        .ok_or_else(|| ExecError::Infrastructure {
            message: format!(
                "container image '{requested}' is absent from the finalized RunLock"
            ),
            fix: "select a statically resolvable image or preserve the run directory and file a Greenlit defect"
                .to_string(),
        })
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
    /// A manifest-sourced `${{ }}` expression (composite step `if`/`run`/
    /// `env`/`with`, `runs.pre-if`/`post-if`, Docker `args`/`env`) failed to
    /// parse or evaluate (`crate::executor::actions::template`).
    #[error("could not evaluate an action expression: {0}")]
    TemplateEval(String),
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
    /// A `uses:` value did not match one of GitHub's four documented forms
    /// (`owner/repo@ref`, `owner/repo/subdir@ref`, `./local/path`,
    /// `docker://image`). The one shape `crate::executor::preflight` still
    /// rejects before any engine work — everything parseable proceeds to
    /// resolution.
    #[error("{span}: `uses: {reference}` {source}")]
    ActionRefInvalid {
        /// The action reference, verbatim.
        reference: String,
        /// Where the `uses:` value was authored.
        span: Span,
        /// The parse failure.
        #[source]
        source: greenlit_actions::UsesRefError,
    },
    /// Hermetic execution encountered a checkout identity that would only be
    /// learned by contacting GitHub after the lock was finalized.
    #[error(
        "{span}: hermetic execution cannot resolve checkout input '{input}' before the first step\n  fix: checkout the frozen current repository, or run without `--hermetic`"
    )]
    HermeticLateInput {
        /// Input name and authored value.
        input: String,
        /// Where the checkout action was authored.
        span: Span,
    },
    /// Resolving a `uses:` ref (tag/branch/SHA) to a commit SHA failed.
    ///
    /// Boxed: `greenlit_actions::resolve::ResolveError` is large enough that
    /// clippy's `result_large_err` flags every `Result<_, ExecError>` return
    /// in this module otherwise (the same reasoning applies to
    /// [`Self::ActionFetch`]/[`Self::ActionManifest`] below).
    #[error("{span}: could not resolve `uses: {reference}`: {source}")]
    ActionResolve {
        /// The action reference, verbatim.
        reference: String,
        /// Where the `uses:` value was authored.
        span: Span,
        /// The underlying resolution failure.
        #[source]
        source: Box<greenlit_actions::resolve::ResolveError>,
    },
    /// Fetching a resolved action's source into the action store failed.
    #[error("{span}: could not fetch `uses: {reference}`: {source}")]
    ActionFetch {
        /// The action reference, verbatim.
        reference: String,
        /// Where the `uses:` value was authored.
        span: Span,
        /// The underlying store/fetch failure.
        #[source]
        source: Box<greenlit_actions::store::StoreError>,
    },
    /// Parsing a resolved action's `action.yml`/`action.yaml` failed.
    #[error("{span}: could not read the manifest for `uses: {reference}`: {source}")]
    ActionManifest {
        /// The action reference, verbatim.
        reference: String,
        /// Where the `uses:` value was authored.
        span: Span,
        /// The underlying manifest parse failure.
        #[source]
        source: Box<greenlit_actions::manifest::ManifestError>,
    },
    /// A checkout of a repository other than the current one was requested
    /// with no token available (`PHASE-3-actions.md` "actions/checkout":
    /// "Checkout of a different repository performs a real clone and
    /// requires a token").
    #[error(
        "{span}: checking out '{repository}' requires a GitHub token, and none is configured\n  fix: run `litci auth` (or `litci auth --pat`/`--gh`), or supply `with: token: <value>` on this step"
    )]
    CheckoutRequiresAuth {
        /// Where the `uses:` value was authored.
        span: Span,
        /// The repository that was requested.
        repository: String,
    },
    /// Runtime matrix materialization failed after dependency outputs existed.
    #[error(
        "could not materialize a runtime matrix: {source}\n  fix: make the producing job emit the documented JSON matrix and scheduling-control types"
    )]
    MatrixRuntime {
        /// The matrix expression, shape, type, or size failure.
        #[source]
        source: greenlit_engine::MatrixError,
    },
    /// A runtime-derived runner label failed evaluation or support validation.
    #[error(
        "could not materialize a runtime runner label: {source}\n  fix: emit one of ubuntu-latest, ubuntu-24.04, or ubuntu-22.04"
    )]
    RunnerRuntime {
        /// The runner-label failure.
        #[source]
        source: greenlit_engine::RunnerError,
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

    /// Wrap a manifest-sourced template parse/evaluation failure
    /// (`crate::executor::actions::template::TemplateError`) — kept as a
    /// `String` (like [`ExecError::CommandFile`]) rather than the typed
    /// error itself, since the typed error lives in a `pub(crate)` module
    /// and this enum is public.
    pub(crate) fn template_eval(source: impl std::fmt::Display) -> Self {
        ExecError::TemplateEval(source.to_string())
    }
}

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
    out: &mut (dyn Write + Send),
    progress: &mut (dyn ProgressSink + Send),
) -> Result<RunReport, ExecError> {
    run_plan_cancellable(
        engine,
        plan,
        config,
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
        &mut logs,
        &mut events,
        progress,
        cancellation,
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
    logs: &mut (dyn crate::events::RunLogSink + Send),
    events: &mut (dyn crate::events::ExecutionEventSink + Send),
    progress: &mut (dyn ProgressSink + Send),
    cancellation: &crate::Cancellation,
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
