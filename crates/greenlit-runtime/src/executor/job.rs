//! Running one job instance end to end: activation, image, container boot,
//! overlay readiness, the sequential step loop, job-output finalization, and
//! teardown.

use std::io::Write;
use std::time::Instant;

use indexmap::IndexMap;
use tracing::Instrument;

use greenlit_engine::execution::env::{EnvLayers, apply_path_additions, layer_step_env};
use greenlit_engine::execution::job_outputs::finalize_outputs;
use greenlit_engine::execution::outcome::{advance_status, job_result_from_status, needs_status};
use greenlit_engine::execution::resolve::{resolve_condition, resolve_string};
use greenlit_engine::execution::{Masker, NeedRecord};
use greenlit_engine::{Conclusion, ContainerPlan, JobId, RunnerImage};
use greenlit_expr::{Context, RunStatus, Value};

use crate::engine::{BindMount, ContainerEngine, ExecSpec};
use crate::executor::container::{ContainerAdditions, ResolvedContainer, validate_container};
use crate::executor::context::{
    CaptureSink, ContextRoots, LiveState, build_context, resolve_env_layer, runner_context,
};
use crate::executor::instance::JobInstance;
use crate::executor::report::{JobReport, StepReport};
use crate::executor::step::{JobRuntime, StepLoopState, execute_step};
use crate::executor::{ExecError, Shared, stage_span};
use crate::image::{INIT_IN_IMAGE_PATH, ensure_base_image, init_binary};
use crate::isolation::isolation_container_spec;
use crate::platform::UbuntuRelease;

/// Base directory for per-step command files inside a container.
const CMDFILES_BASE: &str = "/greenlit/cmdfiles";
/// Readiness marker `greenlit-init` touches once isolation is established.
const READY_MARKER: &str = "/greenlit/ready";

/// The resolved per-job env layers and defaults threaded into the step loop.
struct RunnerLayers<'a> {
    runner_ctx: &'a Value,
    base_env: &'a IndexMap<String, String>,
    workflow_env: &'a IndexMap<String, String>,
    job_env: &'a IndexMap<String, String>,
    default_shell: Option<&'a str>,
    default_wd: Option<&'a str>,
    in_container: bool,
    bash_available: bool,
}

/// Run one job instance and produce its report.
///
/// # Errors
///
/// Returns an [`ExecError`] on any infrastructure or evaluation failure; a
/// step's ordinary non-zero exit is not an error but a recorded result.
pub(crate) async fn run_instance(
    shared: &Shared<'_>,
    masker: &mut Masker,
    instance: &JobInstance<'_>,
    job_id: &JobId,
    needs: &[NeedRecord],
    out: &mut (dyn Write + Send),
) -> Result<JobReport, ExecError> {
    let started = Instant::now();

    let mut runner_env = shared.config.runner_env.clone();
    runner_env.job = job_id.0.clone();
    runner_env.workspace = shared.config.workspace.clone();
    let base_env = runner_env.clone().into_map();
    let runner_ctx = runner_context(&runner_env);

    let needs_run_status = needs_status(needs);
    let activation_ctx = env_ctx(
        shared.roots,
        &runner_ctx,
        &Value::Null,
        &base_env,
        needs,
        needs_run_status,
    );
    // Masked before any print/report, same reasoning as a step's `name:`
    // (`crate::executor::step::execute_step`): a job's display name can
    // interpolate `${{ needs.<id>.outputs.* }}`, and an upstream job may have
    // masked that value via `::add-mask::`.
    let display =
        masker.apply(&resolve_string(instance.display, &activation_ctx).map_err(ExecError::eval)?);

    if !job_activates(instance, needs, &activation_ctx)? {
        let _ = writeln!(out, "\n\u{2022} job {display}: skipped");
        return Ok(skipped_report(job_id, display, started));
    }
    let _ = writeln!(out, "\n\u{2022} job {display}");

    let job_env = resolve_env_and_defaults(
        shared,
        &runner_ctx,
        instance,
        &base_env,
        needs,
        needs_run_status,
    )?;

    let (image_tag, in_container, bash_available, additions) =
        resolve_image(shared, &runner_ctx, instance, &base_env, needs).await?;

    let container = boot_container(shared, &image_tag, in_container, &additions).await?;

    let outcome = run_job_body(
        shared,
        masker,
        instance,
        RunnerLayers {
            runner_ctx: &runner_ctx,
            base_env: &base_env,
            workflow_env: &job_env.workflow_env,
            job_env: &job_env.job_env,
            default_shell: job_env.default_shell.as_deref(),
            default_wd: job_env.default_wd.as_deref(),
            in_container,
            bash_available,
        },
        &container,
        needs,
        out,
    )
    .await;

    // `--write-back` needs the container (and its overlay upper) reachable
    // after the run to export the diff (`PHASE-2-execution.md` "Overlay
    // isolation": "export the upper-layer diff ... after the run"); the
    // caller (`litci run`) removes it once write-back has processed this
    // job. Otherwise, best-effort teardown here and now: a leaked container
    // is not a run failure, and it must not mask the job's real result or
    // error.
    if !shared.config.write_back {
        let _ = shared.engine.remove_container(&container).await;
    }

    let (step_reports, outputs, result) = outcome?;
    Ok(JobReport {
        id: job_id.0.clone(),
        display,
        result,
        steps: step_reports,
        outputs,
        duration: started.elapsed(),
        container_id: shared.config.write_back.then_some(container),
    })
}

/// Run a booted job's steps and finalize its outputs.
async fn run_job_body(
    shared: &Shared<'_>,
    masker: &mut Masker,
    instance: &JobInstance<'_>,
    layers: RunnerLayers<'_>,
    container: &str,
    needs: &[NeedRecord],
    out: &mut (dyn Write + Send),
) -> Result<(Vec<StepReport>, IndexMap<String, String>, Conclusion), ExecError> {
    wait_for_ready(shared.engine, container).await?;

    let job_rt = JobRuntime {
        engine: shared.engine,
        container,
        roots: shared.roots,
        runner_ctx: layers.runner_ctx,
        matrix: &instance.matrix,
        base_env: layers.base_env,
        workflow_env: layers.workflow_env,
        job_env: layers.job_env,
        default_shell: layers.default_shell,
        default_working_directory: layers.default_wd,
        in_container: layers.in_container,
        bash_available: layers.bash_available,
        workspace: &shared.config.workspace,
        cmdfiles_base: CMDFILES_BASE,
    };

    let mut state = StepLoopState::new();
    let mut step_reports = Vec::with_capacity(instance.steps.len());
    for (index, step) in instance.steps.iter().enumerate() {
        let executed = execute_step(&job_rt, needs, step, index, &mut state, masker, out)
            .instrument(stage_span("exec"))
            .await?;
        state.status = advance_status(state.status, executed.result.conclusion);
        step_reports.push(StepReport {
            label: executed.label,
            outcome: executed.result.outcome,
            conclusion: executed.result.conclusion,
            duration: executed.duration,
            ran: executed.ran,
        });
    }

    // Job outputs are evaluated against the job's final `env` context — the full
    // layered environment (workflow + job `env:` and every value written to
    // `GITHUB_ENV` by the job's steps), not just the runner defaults. GitHub's
    // `env` context "contains environment variables that have been set in a
    // workflow, job, or step", and job-output expressions run at job scope after
    // every step, so the accumulated values are in scope.
    // <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#env-context>
    let empty = IndexMap::new();
    let mut final_env = layer_step_env(EnvLayers {
        base: layers.base_env,
        workflow: layers.workflow_env,
        job: layers.job_env,
        accumulated: &state.accumulated,
        step: &empty,
    });
    apply_path_additions(&mut final_env, &state.path_additions);
    let final_ctx = build_context(
        shared.roots,
        &LiveState {
            runner: layers.runner_ctx,
            matrix: &instance.matrix,
            env: &final_env,
            steps: &state.records,
            needs,
            status: state.status,
        },
    );
    let outputs = finalize_outputs(instance.outputs, &final_ctx, masker)?;
    let result = job_result_from_status(state.status);
    Ok((step_reports, outputs, result))
}

/// Whether the job's own gate (static skip, then `if:` with the implicit
/// `success()`) lets it run.
fn job_activates(
    instance: &JobInstance<'_>,
    needs: &[NeedRecord],
    ctx: &Context,
) -> Result<bool, ExecError> {
    if instance.skip.is_some() {
        return Ok(false);
    }
    let condition_true = match instance.condition {
        Some(condition) => resolve_condition(condition, ctx).map_err(ExecError::eval)?,
        None => true,
    };
    if instance.implicit_status_gate {
        // The implicit `success()` requires every dependency to have succeeded;
        // a failed, cancelled, or skipped need suppresses the job.
        let all_succeeded = needs
            .iter()
            .all(|need| matches!(need.result, Conclusion::Success));
        Ok(all_succeeded && condition_true)
    } else {
        // The authored condition uses a status function and already accounts for
        // the needs status through `ctx`.
        Ok(condition_true)
    }
}

/// The resolved workflow/job `env:` layers and the effective run defaults.
struct ResolvedJobEnv {
    workflow_env: IndexMap<String, String>,
    job_env: IndexMap<String, String>,
    default_shell: Option<String>,
    default_wd: Option<String>,
}

/// Resolve the workflow and job `env:` layers and the effective run defaults.
fn resolve_env_and_defaults(
    shared: &Shared<'_>,
    runner_ctx: &Value,
    instance: &JobInstance<'_>,
    base_env: &IndexMap<String, String>,
    needs: &[NeedRecord],
    status: RunStatus,
) -> Result<ResolvedJobEnv, ExecError> {
    let workflow_ctx = env_ctx(
        shared.roots,
        runner_ctx,
        &instance.matrix,
        base_env,
        needs,
        status,
    );
    let workflow_env =
        resolve_env_layer(shared.workflow_env, &workflow_ctx).map_err(ExecError::eval)?;

    let mut base_plus_workflow = base_env.clone();
    for (key, value) in &workflow_env {
        base_plus_workflow.insert(key.clone(), value.clone());
    }
    let job_ctx = env_ctx(
        shared.roots,
        runner_ctx,
        &instance.matrix,
        &base_plus_workflow,
        needs,
        status,
    );
    let job_env = resolve_env_layer(instance.job_env, &job_ctx).map_err(ExecError::eval)?;

    let default_shell = match &instance.defaults.shell {
        Some(planned) => Some(resolve_string(planned, &job_ctx).map_err(ExecError::eval)?),
        None => None,
    };
    let default_wd = match &instance.defaults.working_directory {
        Some(planned) => Some(resolve_string(planned, &job_ctx).map_err(ExecError::eval)?),
        None => None,
    };
    Ok(ResolvedJobEnv {
        workflow_env,
        job_env,
        default_shell,
        default_wd,
    })
}

/// Ensure the job's image exists and, for a job container, validate it.
///
/// Returns `(image_tag, in_container, bash_available, additions)`.
async fn resolve_image(
    shared: &Shared<'_>,
    runner_ctx: &Value,
    instance: &JobInstance<'_>,
    base_env: &IndexMap<String, String>,
    needs: &[NeedRecord],
) -> Result<(String, bool, bool, ContainerAdditions), ExecError> {
    match instance.container {
        Some(container_plan) => {
            let ctx = env_ctx(
                shared.roots,
                runner_ctx,
                &instance.matrix,
                base_env,
                needs,
                RunStatus::Success,
            );
            let resolved = resolve_container(container_plan, &ctx)?;
            let additions = validate_container(
                &resolved,
                &shared.config.workspace,
                &shared.config.volume_namespace,
            )?;
            // Pull only when absent, so a present image (and an offline host)
            // still runs, and re-runs skip the registry round-trip.
            let ensure = async {
                if !shared.engine.image_exists(&resolved.image).await? {
                    shared.engine.pull_image(&resolved.image).await?;
                }
                Ok::<_, ExecError>(())
            };
            ensure.instrument(stage_span("image-ensure")).await?;
            // A job container image is not guaranteed to ship bash; GitHub
            // defaults such jobs to `sh`.
            Ok((resolved.image, true, false, additions))
        }
        None => {
            let release = match instance.runner {
                RunnerImage::Ubuntu2404 => UbuntuRelease::Noble2404,
                RunnerImage::Ubuntu2204 => UbuntuRelease::Jammy2204,
            };
            let tag = ensure_base_image(shared.engine, release)
                .instrument(stage_span("image-ensure"))
                .await?;
            Ok((tag, false, true, ContainerAdditions::default()))
        }
    }
}

/// Resolve a `container:` plan's context-sensitive fields against `ctx`.
fn resolve_container(plan: &ContainerPlan, ctx: &Context) -> Result<ResolvedContainer, ExecError> {
    let image = resolve_string(&plan.image, ctx).map_err(ExecError::eval)?;
    let mut env = Vec::with_capacity(plan.env.len());
    for (name, value) in &plan.env {
        env.push((
            name.clone(),
            resolve_string(value, ctx).map_err(ExecError::eval)?,
        ));
    }
    let mut volumes = Vec::with_capacity(plan.volumes.len());
    for value in &plan.volumes {
        volumes.push((
            resolve_string(value, ctx).map_err(ExecError::eval)?,
            value.span.clone(),
        ));
    }
    let mut ports = Vec::with_capacity(plan.ports.len());
    for value in &plan.ports {
        ports.push((
            resolve_string(value, ctx).map_err(ExecError::eval)?,
            value.span.clone(),
        ));
    }
    let options = match &plan.options {
        Some(value) => Some((
            resolve_string(value, ctx).map_err(ExecError::eval)?,
            value.span.clone(),
        )),
        None => None,
    };
    let credentials_span = plan.credentials.as_ref().and_then(|creds| {
        creds
            .password
            .as_ref()
            .or(creds.username.as_ref())
            .map(|value| value.span.clone())
    });
    Ok(ResolvedContainer {
        image,
        image_span: plan.image.span.clone(),
        has_credentials: plan.credentials.is_some(),
        credentials_span,
        env,
        volumes,
        ports,
        options,
    })
}

/// Create and start the isolated job container, returning its id.
async fn boot_container(
    shared: &Shared<'_>,
    image: &str,
    in_container: bool,
    additions: &ContainerAdditions,
) -> Result<String, ExecError> {
    let repo = shared.config.repo_host_path.to_string_lossy().into_owned();
    let idle = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("touch {READY_MARKER}; exec tail -f /dev/null"),
    ];
    let mut spec = isolation_container_spec(
        image.to_string(),
        &repo,
        &shared.config.workspace,
        shared.config.strategy,
        idle,
    );
    // A job container image does not ship the helper; inject it read-only.
    if in_container {
        let helper = write_helper_binary()?;
        spec.binds.push(BindMount {
            host_path: helper,
            container_path: INIT_IN_IMAGE_PATH.to_string(),
            read_only: true,
        });
    }
    spec.binds.extend(additions.volume_binds.iter().cloned());
    spec.env = additions.env.clone();
    spec.labels = vec![("greenlit.managed".to_string(), "1".to_string())];

    let engine = shared.engine;
    let boot = async {
        let id = engine.create_container(&spec).await?;
        engine.start_container(&id).await?;
        Ok::<_, ExecError>(id)
    };
    boot.instrument(stage_span("container-boot")).await
}

/// Poll until `greenlit-init` signals isolation is established (the overlay or
/// copy-in workspace is live), timing the wait as the `overlay-setup` stage.
async fn wait_for_ready(engine: &dyn ContainerEngine, container: &str) -> Result<(), ExecError> {
    let program = format!(
        "i=0; until [ -e {READY_MARKER} ]; do i=$((i+1)); [ $i -gt 1200 ] && exit 1; sleep 0.05; done"
    );
    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), program],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = CaptureSink::default();
    let output = engine
        .exec(container, &spec, &mut sink)
        .instrument(stage_span("overlay-setup"))
        .await?;
    if output.exit_code != 0 {
        return Err(ExecError::Infrastructure {
            message: "the job container never became ready (isolation setup did not complete)"
                .to_string(),
            fix: "ensure the runner image provides a POSIX `sh` and `tail`".to_string(),
        });
    }
    Ok(())
}

/// Write the embedded `greenlit-init` bytes to a host temp file (mode 0755) so
/// they can be bind-mounted into a job container.
fn write_helper_binary() -> Result<String, ExecError> {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("greenlit-init-{}-{nanos}", std::process::id()));
    std::fs::write(&path, init_binary()).map_err(helper_io_error)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .map_err(helper_io_error)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Map a helper-staging I/O error onto an [`ExecError`] with a fix.
fn helper_io_error(source: std::io::Error) -> ExecError {
    ExecError::Infrastructure {
        message: format!("could not stage the greenlit-init helper: {source}"),
        fix: "ensure the system temporary directory is writable".to_string(),
    }
}

/// Build a context for env/defaults/activation resolution.
fn env_ctx(
    roots: &ContextRoots,
    runner_ctx: &Value,
    matrix: &Value,
    env: &IndexMap<String, String>,
    needs: &[NeedRecord],
    status: RunStatus,
) -> Context {
    build_context(
        roots,
        &LiveState {
            runner: runner_ctx,
            matrix,
            env,
            steps: &[],
            needs,
            status,
        },
    )
}

/// A report for a job whose gate suppressed it.
fn skipped_report(job_id: &JobId, display: String, started: Instant) -> JobReport {
    JobReport {
        id: job_id.0.clone(),
        display,
        result: Conclusion::Skipped,
        steps: Vec::new(),
        outputs: IndexMap::new(),
        duration: started.elapsed(),
        container_id: None,
    }
}
