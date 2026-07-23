//! Executing one `run:` step: finishing its plan-time-deferred values against
//! the live context, resolving its shell, running it as an exec, and folding
//! its command files back into the job's live state.
//!
//! Every GitHub-faithful decision here delegates to the engine's execution
//! semantics ([`greenlit_engine::execution`]): shell resolution, env layering,
//! the `outcome`/`conclusion` model, and the implicit `success()` gate. This
//! module is the runtime driver that turns those decisions into container execs.

use std::io::Write;
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use greenlit_engine::execution::env::{EnvLayers, apply_path_additions, layer_step_env};
use greenlit_engine::execution::outcome::{
    StepExit, StepResult, step_activates, step_result_from_exit, step_result_skipped,
};
use greenlit_engine::execution::resolve::{
    resolve_bool, resolve_condition, resolve_minutes, resolve_string,
};
use greenlit_engine::execution::shell::{ShellSelection, resolve_shell};
use greenlit_engine::execution::{Masker, NeedRecord, StepRecord};
use greenlit_engine::{Conclusion, StepKind, StepPlan};
use greenlit_expr::{RunStatus, Value};

use crate::engine::{ContainerEngine, ExecSpec};
use crate::executor::ExecError;
use crate::executor::cmdfiles::{self, CommandFilePaths};
use crate::executor::context::{ContextRoots, LiveState, build_context, resolve_env_layer};
use crate::executor::logsink::StepLogSink;

/// Per-job constants a step needs — the container, the resolved env layers, the
/// job defaults, and the run-wide context roots.
pub(crate) struct JobRuntime<'a> {
    /// The container engine.
    pub engine: &'a dyn ContainerEngine,
    /// The running job container's id.
    pub container: &'a str,
    /// Run-wide context roots.
    pub roots: &'a ContextRoots,
    /// The `runner` context object.
    pub runner_ctx: &'a Value,
    /// The `matrix` context value.
    pub matrix: &'a Value,
    /// The runner/base env layer (`GITHUB_*`/`RUNNER_*`).
    pub base_env: &'a IndexMap<String, String>,
    /// The resolved workflow `env:` layer.
    pub workflow_env: &'a IndexMap<String, String>,
    /// The resolved job `env:` layer.
    pub job_env: &'a IndexMap<String, String>,
    /// The effective `defaults.run.shell`, if any.
    pub default_shell: Option<&'a str>,
    /// The effective `defaults.run.working-directory`, if any.
    pub default_working_directory: Option<&'a str>,
    /// Whether this job runs in a `container:` image (selects `sh` defaults).
    pub in_container: bool,
    /// Whether `bash` is available on the image (base image: yes).
    pub bash_available: bool,
    /// `GITHUB_WORKSPACE` inside the container.
    pub workspace: &'a str,
    /// Base directory for this job's per-step command files.
    pub cmdfiles_base: &'a str,
}

/// The job's evolving state across its steps.
pub(crate) struct StepLoopState {
    /// `GITHUB_ENV` accumulation, overriding workflow/job env for later steps.
    pub accumulated: IndexMap<String, String>,
    /// `GITHUB_PATH` additions, highest-priority first.
    pub path_additions: Vec<String>,
    /// The `steps` context contributions so far.
    pub records: Vec<StepRecord>,
    /// The rolling job status.
    pub status: RunStatus,
}

impl StepLoopState {
    /// A fresh state for a job that is about to start.
    pub fn new() -> Self {
        StepLoopState {
            accumulated: IndexMap::new(),
            path_additions: Vec::new(),
            records: Vec::new(),
            status: RunStatus::Success,
        }
    }
}

/// One step's execution result, for the run report.
pub(crate) struct ExecutedStep {
    /// The `outcome`/`conclusion` pair.
    pub result: StepResult,
    /// The display label used in the log and table.
    pub label: String,
    /// Wall-clock exec duration (zero for a skipped step).
    pub duration: Duration,
    /// Whether the body ran.
    pub ran: bool,
}

/// Execute step `index`, mutating the job's live state and streaming its log.
///
/// # Errors
///
/// Returns [`ExecError`] on an engine failure, an unfinished expression, an
/// unsupported shell, a `uses:` step (Phase 3), or a malformed command file.
pub(crate) async fn execute_step(
    job: &JobRuntime<'_>,
    needs: &[NeedRecord],
    step: &StepPlan,
    index: usize,
    state: &mut StepLoopState,
    masker: &mut Masker,
    out: &mut (dyn Write + Send),
) -> Result<ExecutedStep, ExecError> {
    let empty = IndexMap::new();
    let mut pre_env = layer_step_env(EnvLayers {
        base: job.base_env,
        workflow: job.workflow_env,
        job: job.job_env,
        accumulated: &state.accumulated,
        step: &empty,
    });
    apply_path_additions(&mut pre_env, &state.path_additions);

    let pre_ctx = build_context(
        job.roots,
        &LiveState {
            runner: job.runner_ctx,
            matrix: job.matrix,
            env: &pre_env,
            steps: &state.records,
            needs,
            status: state.status,
        },
    );

    let label = step_label(step, index, &pre_ctx)?;

    let condition_true = match &step.condition {
        Some(condition) => resolve_condition(condition, &pre_ctx).map_err(ExecError::eval)?,
        None => true,
    };
    if !step_activates(state.status, step.implicit_status_gate, condition_true) {
        record_skip(step, &mut state.records);
        let _ = writeln!(out, "  \u{2013} {label} (skipped)");
        return Ok(ExecutedStep {
            result: step_result_skipped(),
            label,
            duration: Duration::ZERO,
            ran: false,
        });
    }

    let (script_planned, step_shell_planned) = match &step.kind {
        StepKind::Run { script, shell } => (script.as_ref(), shell.as_ref()),
        StepKind::Uses { reference, .. } => {
            return Err(ExecError::UsesUnsupported {
                reference: reference.clone(),
            });
        }
    };

    let step_env = resolve_env_layer(&step.env, &pre_ctx).map_err(ExecError::eval)?;
    let mut full_env = layer_step_env(EnvLayers {
        base: job.base_env,
        workflow: job.workflow_env,
        job: job.job_env,
        accumulated: &state.accumulated,
        step: &step_env,
    });
    apply_path_additions(&mut full_env, &state.path_additions);

    let ctx = build_context(
        job.roots,
        &LiveState {
            runner: job.runner_ctx,
            matrix: job.matrix,
            env: &full_env,
            steps: &state.records,
            needs,
            status: state.status,
        },
    );

    let paths = CommandFilePaths::new(job.cmdfiles_base, index);
    let step_shell = match step_shell_planned {
        Some(planned) => Some(resolve_string(planned, &ctx).map_err(ExecError::eval)?),
        None => None,
    };
    let shell = resolve_shell(
        ShellSelection {
            step_shell: step_shell.as_deref(),
            default_shell: job.default_shell,
            in_container: job.in_container,
            bash_available: job.bash_available,
        },
        &paths.script,
    )
    .map_err(|source| ExecError::Shell {
        label: label.clone(),
        source,
    })?;
    let script = resolve_string(script_planned, &ctx).map_err(ExecError::eval)?;
    let working_dir = resolve_working_dir(step, job, &ctx)?;
    let continue_on_error = match &step.continue_on_error {
        Some(planned) => resolve_bool(planned, &ctx).map_err(ExecError::eval)?,
        None => false,
    };
    let timeout = match &step.timeout_minutes {
        Some(planned) => Some(resolve_minutes(planned, &ctx).map_err(ExecError::eval)?),
        None => None,
    };

    cmdfiles::prepare(job.engine, job.container, &paths, &script)
        .await
        .map_err(ExecError::CommandFile)?;

    let _ = writeln!(out, "\u{25b6} {label}");
    let exec_env = exec_env_vec(&full_env, &paths);
    let spec = ExecSpec {
        cmd: std::iter::once(shell.program)
            .chain(shell.args)
            .collect::<Vec<_>>(),
        env: exec_env,
        working_dir: Some(working_dir),
    };

    let started = Instant::now();
    let exit = run_exec(job.engine, job.container, &spec, timeout, out, masker).await?;
    let duration = started.elapsed();

    let effects = cmdfiles::collect(job.engine, job.container, &paths)
        .await
        .map_err(ExecError::CommandFile)?;
    for assignment in &effects.env {
        state
            .accumulated
            .insert(assignment.name.clone(), assignment.value.clone());
    }
    if !effects.path_additions.is_empty() {
        let mut merged = effects.path_additions.clone();
        merged.append(&mut state.path_additions);
        state.path_additions = merged;
    }
    let mut outputs = IndexMap::new();
    for assignment in &effects.outputs {
        outputs.insert(assignment.name.clone(), assignment.value.clone());
    }
    if !effects.summary_within_limit {
        // GitHub drops an over-cap job summary and notes it; mirror that.
        let _ = writeln!(
            out,
            "  [warning] {label}: GITHUB_STEP_SUMMARY exceeded the 1 MiB limit and was dropped"
        );
    }

    let result = step_result_from_exit(exit, continue_on_error);
    if let Some(id) = &step.id {
        state.records.push(StepRecord {
            id: id.clone(),
            outcome: result.outcome,
            conclusion: result.conclusion,
            outputs,
        });
    }
    let _ = writeln!(out, "  {} {label}", status_sigil(result.conclusion));
    Ok(ExecutedStep {
        result,
        label,
        duration,
        ran: true,
    })
}

/// Run the step exec, streaming output through a [`StepLogSink`], honoring an
/// optional per-step timeout.
async fn run_exec(
    engine: &dyn ContainerEngine,
    container: &str,
    spec: &ExecSpec,
    timeout_minutes: Option<f64>,
    out: &mut (dyn Write + Send),
    masker: &mut Masker,
) -> Result<StepExit, ExecError> {
    let mut sink = StepLogSink::new(out, masker);
    let exit = match timeout_minutes {
        Some(minutes) => {
            let duration = Duration::from_secs_f64((minutes * 60.0).max(0.0));
            match tokio::time::timeout(duration, engine.exec(container, spec, &mut sink)).await {
                Ok(result) => exit_from(result?),
                // A timed-out step is a failure (GitHub kills it); the dropped
                // future stops streaming and container teardown reaps it.
                Err(_elapsed) => StepExit::TimedOut,
            }
        }
        None => exit_from(engine.exec(container, spec, &mut sink).await?),
    };
    sink.finish();
    Ok(exit)
}

/// Map a completed exec's exit code onto the step-exit model.
fn exit_from(output: crate::engine::ExecOutput) -> StepExit {
    if output.exit_code == 0 {
        StepExit::Success
    } else {
        StepExit::Failed
    }
}

/// Resolve the step's working directory to an absolute container path.
fn resolve_working_dir(
    step: &StepPlan,
    job: &JobRuntime<'_>,
    ctx: &greenlit_expr::Context,
) -> Result<String, ExecError> {
    let authored = match &step.working_directory {
        Some(planned) => Some(resolve_string(planned, ctx).map_err(ExecError::eval)?),
        None => job.default_working_directory.map(str::to_string),
    };
    Ok(match authored {
        Some(dir) if dir.starts_with('/') => dir,
        Some(dir) => format!("{}/{dir}", job.workspace.trim_end_matches('/')),
        None => job.workspace.to_string(),
    })
}

/// Build the exec environment vector, layering in the four command-file paths.
fn exec_env_vec(
    full_env: &IndexMap<String, String>,
    paths: &CommandFilePaths,
) -> Vec<(String, String)> {
    let mut env = full_env.clone();
    env.insert("GITHUB_ENV".to_string(), paths.env.clone());
    env.insert("GITHUB_OUTPUT".to_string(), paths.output.clone());
    env.insert("GITHUB_PATH".to_string(), paths.path.clone());
    env.insert("GITHUB_STEP_SUMMARY".to_string(), paths.summary.clone());
    env.into_iter().collect()
}

/// Record a skipped step in the `steps` context (only steps with an `id:`
/// appear, matching GitHub).
fn record_skip(step: &StepPlan, records: &mut Vec<StepRecord>) {
    if let Some(id) = &step.id {
        let skipped = step_result_skipped();
        records.push(StepRecord {
            id: id.clone(),
            outcome: skipped.outcome,
            conclusion: skipped.conclusion,
            outputs: IndexMap::new(),
        });
    }
}

/// Compute a step's display label: its `name:` (resolved), else `id:`, else a
/// positional label.
fn step_label(
    step: &StepPlan,
    index: usize,
    ctx: &greenlit_expr::Context,
) -> Result<String, ExecError> {
    if let Some(name) = &step.name {
        return resolve_string(name, ctx).map_err(ExecError::eval);
    }
    if let Some(id) = &step.id {
        return Ok(id.clone());
    }
    Ok(format!("step {}", index + 1))
}

/// A short sigil for a step's conclusion in the live status line.
fn status_sigil(conclusion: Conclusion) -> &'static str {
    match conclusion {
        Conclusion::Success => "\u{2713}",
        Conclusion::Failure => "\u{2717}",
        Conclusion::Cancelled => "\u{29b8}",
        Conclusion::Skipped => "\u{2013}",
    }
}
