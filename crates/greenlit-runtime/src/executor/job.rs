//! Running one job instance end to end: activation, image, container boot,
//! overlay readiness, the sequential step loop, job-output finalization, and
//! teardown.

use std::io::Write;
use std::time::Instant;

use indexmap::IndexMap;
use tracing::Instrument;

use greenlit_engine::execution::env::{EnvLayers, RunnerEnv, apply_path_additions, layer_step_env};
use greenlit_engine::execution::job_outputs::finalize_outputs;
use greenlit_engine::execution::outcome::{advance_status, job_result_from_status, needs_status};
use greenlit_engine::execution::resolve::{resolve_condition, resolve_string};
use greenlit_engine::execution::{Masker, NeedRecord};
use greenlit_engine::{Conclusion, ContainerPlan, JobId, RunnerImage};
use greenlit_expr::{Context, RunStatus, Value};

use crate::engine::{BindMount, ContainerEngine, ExecSpec};
use crate::executor::actions::node_runtime;
use crate::executor::actions::nodejs::NodeRuntimeMounts;
use crate::executor::actions::resolve::{JobActionPlan, resolve_job_actions};
use crate::executor::container::{
    ContainerAdditions, ResolvedContainer, ResolvedCredentials, namespaced_volume_name,
    validate_container,
};
use crate::executor::context::{
    CaptureSink, ContextRoots, LiveState, build_context, resolve_env_layer, runner_context,
};
use crate::executor::dind;
use crate::executor::instance::JobInstance;
use crate::executor::netguard;
use crate::executor::provision;
use crate::executor::readiness::{self, READY_MARKER};
use crate::executor::report::{JobReport, StepReport};
use crate::executor::services;
use crate::executor::step::{
    JobRuntime, StepLoopState, execute_step, run_post_steps, run_pre_steps,
};
use crate::executor::{ExecError, Shared, stage_span};
use crate::image::{INIT_IN_IMAGE_PATH, ensure_base_image, init_binary};
use crate::isolation::{IsolationStrategy, isolation_container_spec};
use crate::platform::UbuntuRelease;
use crate::progress::{ProgressEvent, ProgressSink};

/// Base directory for per-step command files inside a container.
const CMDFILES_BASE: &str = "/greenlit/cmdfiles";

/// Cap on the `PATH` read back from an untrusted job image.
///
/// Generous next to any real `PATH` (a stock Ubuntu image is well under 100
/// bytes), while bounding what a hostile image can make the host hold.
const MAX_PATH_BYTES: usize = 32 * 1024;

/// The bare (pre-namespacing) name of the run-scoped named volume backing a
/// job's shared workspace when it contains a Docker action
/// (`crate::executor::actions::docker_action` module docs) — namespaced the
/// same way a workflow-authored `volumes:` entry is
/// (`crate::executor::container::namespaced_volume_name`).
const DOCKER_SIBLING_WORKSPACE_VOLUME: &str = "workspace";

/// What `resolve_image` settled on for this job.
struct ResolvedImage {
    /// The image the container actually starts from — this repository's
    /// converged image when one exists, else the base.
    tag: String,
    /// The *base* image the convergence is anchored to.
    ///
    /// Kept separate from `tag` because the converged tag is hashed over it:
    /// hashing over `tag` instead would produce a new tag every time a
    /// converged image was itself converged, so a repository would never
    /// reuse one. `None` for a user-declared `container:`, which never
    /// converges.
    base: Option<String>,
    /// Whether this is a user-declared `container:`.
    in_container: bool,
    /// Whether `bash` is known to be present.
    bash_available: bool,
    /// Validated job-container additions.
    additions: ContainerAdditions,
}

/// The resolved per-job env layers and defaults threaded into the step loop,
/// plus everything action execution needs beyond that.
struct RunnerLayers<'a> {
    runner_ctx: &'a Value,
    base_env: &'a IndexMap<String, String>,
    workflow_env: &'a IndexMap<String, String>,
    job_env: &'a IndexMap<String, String>,
    default_shell: Option<&'a str>,
    default_wd: Option<&'a str>,
    in_container: bool,
    bash_available: bool,
    action_plan: &'a JobActionPlan,
    node_mounts: &'a NodeRuntimeMounts,
    docker_workspace_volume: Option<&'a str>,
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
    progress: &mut (dyn ProgressSink + Send),
) -> Result<JobReport, ExecError> {
    let started = Instant::now();

    let mut runner_env = shared.config.runner_env.clone();
    runner_env.job = job_id.0.clone();
    runner_env.workspace = shared.config.workspace.clone();

    // The job's own bridge: the shim binds on its gateway and, from the next
    // task group, services attach to it. Created before the container so the
    // container can be attached at creation time, and torn down after it --
    // a network with an attached container cannot be removed.
    let network_name = format!(
        "greenlit-run-{}-{}",
        shared.config.volume_namespace, job_id.0
    );
    let job_network =
        services::create(shared.engine, &network_name, shared.config.store.as_ref()).await?;
    runner_env.actions_service = job_network.actions_service().cloned();

    let mut base_env = runner_env.clone().into_map();
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

    // Resolved *before* the container boots: every read-only bind the job's
    // `uses:` steps need (fetched action source, pinned Node runtimes) and
    // whether the job needs a Docker-action sibling — which decides the
    // workspace isolation strategy below — must be known at container-create
    // time (`crate::executor::actions` module docs).
    let action_plan = resolve_job_actions(
        instance.steps,
        &shared.config.actions,
        &shared.config.repo_host_path,
        &shared.config.workspace,
    )
    .await?;
    let (node_binds, node_mounts) = if action_plan.needs_node20 || action_plan.needs_node24 {
        progress.on_progress(ProgressEvent::ActionRuntimeEnsureStarted);
        let ensured = node_runtime::ensure_mounts(
            &shared.config.actions.node_runtime_store,
            shared.config.actions.node_runtime_fetcher.as_ref(),
            shared.config.actions.node_runtime_specs.as_ref(),
            action_plan.needs_node20,
            action_plan.needs_node24,
        )
        .instrument(stage_span("action-runtime-ensure"))
        .await
        .map_err(|source| ExecError::Infrastructure {
            message: format!("could not prepare a pinned Node action runtime: {source}"),
            fix: "check network connectivity and retry".to_string(),
        })?;
        progress.on_progress(ProgressEvent::ActionRuntimeEnsureFinished);
        ensured
    } else {
        (Vec::new(), NodeRuntimeMounts::default())
    };
    let mut extra_binds = action_plan.binds.clone();
    extra_binds.extend(node_binds);

    // Services start before the job container, exactly as GitHub starts them
    // before the job: a first step that connects to `postgres:5432` must find
    // it already listening, not race it.
    //
    // Both halves unwind the network on failure. The executor has no
    // `Drop`-based cleanup registry, so anything created before the container
    // has to be released on every path out — a rejected service definition
    // leaked an empty network until this was folded into one place.
    let services_started =
        match resolve_services(shared, masker, &runner_ctx, instance, &base_env, needs) {
            Ok(resolved) => {
                match services::start(
                    shared.engine,
                    job_network.name(),
                    job_network.policy(),
                    &resolved,
                    progress,
                )
                .await
                {
                    Ok(started) => started,
                    Err(failure) => {
                        // Some may already be running; stop them before removing
                        // the network they are attached to.
                        services::stop(shared.engine, &failure.started).await;
                        job_network.teardown(shared.engine).await;
                        return Err(failure.cause);
                    }
                }
            }
            Err(error) => {
                job_network.teardown(shared.engine).await;
                return Err(error);
            }
        };

    let resolved_image = resolve_image(
        shared,
        masker,
        &runner_ctx,
        instance,
        &base_env,
        needs,
        progress,
    )
    .await?;
    let ResolvedImage {
        tag: image_tag,
        base: base_image_tag,
        in_container,
        bash_available,
        additions,
    } = resolved_image;

    let container = boot_container(
        shared,
        &BootRequest {
            image: &image_tag,
            in_container,
            additions: &additions,
            extra_binds: &extra_binds,
            needs_docker_sibling: action_plan.needs_docker_sibling,
            network: job_network.name(),
        },
        progress,
    )
    .await?;

    // The network policy goes on before readiness, and therefore before any
    // workflow code runs: a container that has executed even one step under
    // an unrestricted network has already had its chance to reach the LAN.
    if let Err(error) =
        netguard::apply(shared.engine, &container, job_network.policy(), progress).await
    {
        let _ = shared.engine.remove_container(&container).await;
        services::stop(shared.engine, &services_started).await;
        job_network.teardown(shared.engine).await;
        return Err(error.into());
    }

    // Docker-in-Docker, only when the job's own scripts call `docker`.
    // Starting a privileged daemon for every job would cost seconds and a
    // privileged container for a capability most workflows never use.
    let dind = if dind::job_uses_docker(instance.steps.iter().filter_map(step_script)) {
        match dind::start(shared.engine, job_network.name(), progress).await {
            Ok(sidecar) => Some(sidecar),
            Err(error) => {
                let _ = shared.engine.remove_container(&container).await;
                services::stop(shared.engine, &services_started).await;
                job_network.teardown(shared.engine).await;
                return Err(error);
            }
        }
    } else {
        None
    };
    if dind.is_some() {
        // The wrapper is installed before any step runs, so the very first
        // `docker` invocation already reaches the sidecar -- the brief is
        // explicit that a missing daemon must never be discovered by
        // rerunning the user's step.
        let install = ExecSpec {
            cmd: vec![
                "sh".to_string(),
                "-c".to_string(),
                dind::install_wrapper_script(),
            ],
            env: Vec::new(),
            working_dir: None,
        };
        if let Err(error) = shared
            .engine
            .exec(&container, &install, &mut crate::engine::SinkNull)
            .await
        {
            let _ = shared.engine.remove_container(&container).await;
            services::stop(shared.engine, &services_started).await;
            job_network.teardown(shared.engine).await;
            return Err(error.into());
        }
    }

    // Set when this job is eligible to converge -- a Greenlit runner image
    // with a store to cache the manifest in.
    let mut converged_target: Option<String> = None;
    // The manifest and base image this job converged against, kept so the
    // converged image can be built from a clean base container at teardown.
    let mut converged_source: Option<(provision::manifest::Manifest, String)> = None;

    // Lazy provisioning, for Greenlit runner images only. A user-declared
    // `container:` is left exactly as authored: a command missing from that
    // image must fail the way it fails in the same job container on GitHub
    // (`PHASE-4-environment.md`: "Never inject host-runner tools or
    // convergence layers into a user-declared job container").
    if !in_container && let Some(store) = shared.config.store.as_ref() {
        let version = match instance.runner {
            RunnerImage::Ubuntu2404 => "24.04",
            RunnerImage::Ubuntu2204 => "22.04",
        };
        let label = instance.runner.image_identifier();
        if let Some(base) = base_image_tag.as_deref() {
            converged_target = Some(provision::converged_tag(
                &shared.config.repo_host_path,
                base,
                label,
            ));
        }
        // A manifest that cannot be reached costs provisioning, not the run:
        // the job proceeds against the slim base, and a genuinely missing
        // command still fails at the step with its own message.
        match provision::fetch::load(&store.toolcache_root_parent(), version)
            .instrument(stage_span("manifest"))
            .await
        {
            Ok(manifest) => {
                let wanted =
                    provision::mentioned_commands(instance.steps.iter().filter_map(step_script));
                if let Some(base) = base_image_tag.clone() {
                    converged_source = Some((manifest.clone(), base));
                }
                if let Err(error) =
                    provision::install_shims(shared.engine, &container, &manifest, &wanted, label)
                        .instrument(stage_span("provision"))
                        .await
                {
                    tracing::debug!(
                        target: "greenlit_runtime::provision",
                        %error,
                        "could not install provisioning shims; continuing without them"
                    );
                }
            }
            Err(error) => tracing::debug!(
                target: "greenlit_runtime::provision",
                %error,
                "could not load the runner-images manifest; continuing without provisioning"
            ),
        }
    }

    // Readiness runs against the freshly booted container, before the step
    // loop; its error still flows through the teardown below rather than
    // returning early and leaking the container.
    let ready = readiness::wait_for_ready(
        shared.engine,
        &container,
        &shared.config.readiness,
        progress,
    )
    .instrument(stage_span("overlay-setup"))
    .await;
    let docker_workspace_volume = action_plan
        .needs_docker_sibling
        .then_some(DOCKER_SIBLING_WORKSPACE_VOLUME);
    let outcome = match ready {
        Ok(()) => match seed_container_path(shared.engine, &container, &mut base_env).await {
            Ok(()) => {
                run_job_body(
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
                        action_plan: &action_plan,
                        node_mounts: &node_mounts,
                        docker_workspace_volume,
                    },
                    (&container, &runner_env),
                    needs,
                    out,
                )
                .await
            }
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };

    // `--write-back` needs the container (and its overlay upper) reachable
    // after the run to export the diff (`PHASE-2-execution.md` "Overlay
    // isolation": "export the upper-layer diff ... after the run"); the
    // caller (`litci run`) removes it once write-back has processed this
    // job. Otherwise, best-effort teardown here and now: a leaked container
    // is not a run failure, and it must not mask the job's real result or
    // error.
    // Convergence happens only for a green job on a Greenlit runner image:
    // committing a container whose steps failed would bake a half-installed
    // state into the image every later run starts from.
    if !in_container
        && matches!(&outcome, Ok((_, _, Conclusion::Success)))
        && let Some(tag) = &converged_target
        && let Some((manifest, base_image)) = &converged_source
    {
        let installed = provision::provisioned_commands(shared.engine, &container).await;
        provision::build_converged(shared.engine, base_image, manifest, &installed, tag).await;
    }

    if !shared.config.write_back {
        let _ = shared.engine.remove_container(&container).await;
        // The Docker-sibling workspace volume outlives the container that
        // bound it, so removing the container is not enough. Before
        // `remove_volume` existed on the port these accumulated on the host
        // until an operator ran `docker volume prune` -- the module doc in
        // `actions::docker_action` already described a per-job removal that
        // no code performed. Removal must follow the container, because a
        // volume still in use cannot be removed.
        if let Some(source) = docker_workspace_volume {
            let volume = namespaced_volume_name(&shared.config.volume_namespace, source);
            let _ = shared.engine.remove_volume(&volume).await;
        }
    }
    // After the container, before the network: services and the DinD sidecar
    // hold attachments too, and a network with any attachment cannot be
    // removed.
    if let Some(sidecar) = &dind {
        let _ = shared.engine.remove_container(sidecar.container()).await;
    }
    services::stop(shared.engine, &services_started).await;
    job_network.teardown(shared.engine).await;

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

/// Run a ready job's steps and finalize its outputs.
async fn run_job_body(
    shared: &Shared<'_>,
    masker: &mut Masker,
    instance: &JobInstance<'_>,
    layers: RunnerLayers<'_>,
    (container, runner_env): (&str, &RunnerEnv),
    needs: &[NeedRecord],
    out: &mut (dyn Write + Send),
) -> Result<(Vec<StepReport>, IndexMap<String, String>, Conclusion), ExecError> {
    let job_rt = JobRuntime {
        engine: shared.engine,
        container,
        roots: shared.roots,
        runner_ctx: layers.runner_ctx,
        runner_env,
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
        action_plan: layers.action_plan,
        action_config: &shared.config.actions,
        node_mounts: layers.node_mounts,
        volume_namespace: &shared.config.volume_namespace,
        docker_workspace_volume: layers.docker_workspace_volume,
    };

    let mut state = StepLoopState::new();
    // Front-loaded, job-wide: every top-level action's `pre:` script runs
    // before the job's first step, in action (job step) order
    // (`crate::executor::step::run_pre_steps` module docs).
    run_pre_steps(&job_rt, needs, instance.steps, &mut state, masker, out)
        .instrument(stage_span("exec"))
        .await?;

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

    // Drained regardless of the job's rolling status — "post steps run in
    // REVERSE order at job end REGARDLESS of failure"
    // (`crate::executor::step::run_post_steps` module docs).
    let post_executed = run_post_steps(&job_rt, needs, &mut state, masker, out)
        .instrument(stage_span("exec"))
        .await?;
    for executed in post_executed {
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
    masker: &mut Masker,
    runner_ctx: &Value,
    instance: &JobInstance<'_>,
    base_env: &IndexMap<String, String>,
    needs: &[NeedRecord],
    progress: &mut (dyn ProgressSink + Send),
) -> Result<ResolvedImage, ExecError> {
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
            // A resolved registry password/username may not already be a
            // registered secret (e.g. a literal in `credentials:`, however
            // inadvisable) — mask both before anything derived from them
            // (the pull's progress events, a later error) can reach output,
            // matching "never log them" (`PHASE-3-actions.md`).
            if let Some(credentials) = &resolved.credentials {
                masker.add(&credentials.username);
                masker.add(&credentials.password);
            }
            let additions = validate_container(
                &resolved,
                &shared.config.workspace,
                &shared.config.volume_namespace,
            )?;
            // Pull only when absent, so a present image (and an offline host)
            // still runs, and re-runs skip the registry round-trip. The image
            // reference is expression-resolved, so it is masked before it can
            // reach a progress display.
            let masked_image = masker.apply(&resolved.image);
            let ensure = async {
                if !shared.engine.image_exists(&resolved.image).await? {
                    let mut masked = MaskedPullSink {
                        inner: progress,
                        masked_image,
                    };
                    shared
                        .engine
                        .pull_image(
                            &resolved.image,
                            additions.registry_auth.as_ref(),
                            &mut masked,
                        )
                        .await?;
                }
                Ok::<_, ExecError>(())
            };
            ensure.instrument(stage_span("image-ensure")).await?;
            // A job container image is not guaranteed to ship bash; GitHub
            // defaults such jobs to `sh`.
            Ok(ResolvedImage {
                tag: resolved.image,
                base: None,
                in_container: true,
                bash_available: false,
                additions,
            })
        }
        None => {
            let release = match instance.runner {
                RunnerImage::Ubuntu2404 => UbuntuRelease::Noble2404,
                RunnerImage::Ubuntu2204 => UbuntuRelease::Jammy2204,
            };
            let base_tag = ensure_base_image(shared.engine, release, progress)
                .instrument(stage_span("image-ensure"))
                .await?;
            // A previous run of this checkout may already have installed the
            // tools its workflows use; starting from that image is what makes
            // convergence pay off (`PHASE-4-environment.md`: "subsequent runs
            // start from it").
            let converged = provision::converged_tag(
                &shared.config.repo_host_path,
                &base_tag,
                instance.runner.image_identifier(),
            );
            let tag = if shared
                .engine
                .image_exists(&converged)
                .await
                .unwrap_or(false)
            {
                converged
            } else {
                base_tag.clone()
            };
            Ok(ResolvedImage {
                tag,
                base: Some(base_tag),
                in_container: false,
                bash_available: true,
                additions: ContainerAdditions::default(),
            })
        }
    }
}

/// Replaces the image reference in pull events with its masked form before
/// forwarding — the reference can interpolate `::add-mask::`ed values.
struct MaskedPullSink<'a> {
    inner: &'a mut (dyn ProgressSink + Send),
    masked_image: String,
}

impl ProgressSink for MaskedPullSink<'_> {
    fn on_progress(&mut self, event: ProgressEvent) {
        let event = match event {
            ProgressEvent::PullStarted { .. } => ProgressEvent::PullStarted {
                image: self.masked_image.clone(),
            },
            ProgressEvent::PullFinished { .. } => ProgressEvent::PullFinished {
                image: self.masked_image.clone(),
            },
            other => other,
        };
        self.inner.on_progress(event);
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
    let credentials = match &plan.credentials {
        Some(creds) => Some(ResolvedCredentials {
            username: match &creds.username {
                Some(value) => resolve_string(value, ctx).map_err(ExecError::eval)?,
                None => String::new(),
            },
            password: match &creds.password {
                Some(value) => resolve_string(value, ctx).map_err(ExecError::eval)?,
                None => String::new(),
            },
        }),
        None => None,
    };
    Ok(ResolvedContainer {
        image,
        image_span: plan.image.span.clone(),
        credentials,
        env,
        volumes,
        ports,
        options,
    })
}

/// Queries the freshly booted (and now ready) job container's own default
/// `PATH` — inherited from the image, before any step or `GITHUB_PATH`
/// mutation — and seeds it into `base_env`.
///
/// Every later step's environment is built by
/// [`greenlit_engine::execution::env::layer_step_env`] plus
/// [`apply_path_additions`], and that function's documented contract is to
/// *prepend* `GITHUB_PATH` additions onto whatever `PATH` the layered map
/// already carries — never to invent one. Before this seed, no layer ever
/// carried an explicit `PATH` key at all (`base_env` is built from
/// [`RunnerEnv::into_map`], which has no `PATH` field), so the container's
/// real `PATH` was reaching every step only through ordinary Docker `exec`
/// environment inheritance, never through Greenlit's own layering. That
/// works for the *first* step of a job, but the instant one step calls
/// `core.addPath()` (`setup-node` and most `setup-*` actions do), the caller
/// starts passing an explicit `PATH=<additions>` entry into every later
/// `exec`, which *overrides* the container's inherited default outright
/// (Docker merges an exec's explicit env over the container's live
/// environment key-for-key) — silently dropping `/usr/bin` and friends from
/// every subsequent step. GitHub's real runner never has this gap: its
/// `ExecutionContext` tracks one explicit, always-current `PATH` value from
/// the job's own start (seeded from the runner process's own environment),
/// which `core.addPath` only ever prepends onto. Querying the booted
/// container's own `PATH` once, up front, and seeding it into `base_env`
/// gives Greenlit's layering that same explicit, always-current baseline —
/// container-agnostic, so it works identically for the convergent base image
/// and an arbitrary user-specified `jobs.<id>.container`.
///
/// # Errors
///
/// Returns an [`ExecError`] if the query exec itself could not be dispatched
/// (an infrastructure failure, not a step failure) — a non-zero exit or
/// empty output is treated as "no baseline available" rather than a hard
/// failure, leaving `base_env` without `PATH` (the pre-fix behavior).
async fn seed_container_path(
    engine: &dyn ContainerEngine,
    container: &str,
    base_env: &mut IndexMap<String, String>,
) -> Result<(), ExecError> {
    let spec = ExecSpec {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s' \"$PATH\"".to_string(),
        ],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = CaptureSink::default();
    let output = engine.exec(container, &spec, &mut sink).await?;
    if output.exit_code == 0 {
        let mut path = sink.text();
        // The image is untrusted, and this read was previously unbounded --
        // every HTTP path in the codebase caps its body read for the same
        // reason. A `PATH` longer than this is not a real one.
        if path.len() > MAX_PATH_BYTES {
            path.truncate(MAX_PATH_BYTES);
            // Truncation can land mid-entry; drop the partial tail rather
            // than leaving a directory name that means something else.
            if let Some(last) = path.rfind(':') {
                path.truncate(last);
            }
        }
        if !path.is_empty() {
            // Wrappers first, provisioning shims last. The managed `docker`
            // wrapper has to beat a CLI the job image already ships (a
            // `docker:27-cli` job container has one at `/usr/local/bin`);
            // a provisioning shim must lose to a real tool the workflow
            // installs, because `apply_path_additions` only ever prepends
            // and treats this whole string as one opaque tail segment.
            base_env.insert(
                "PATH".to_string(),
                format!("{}:{path}:{}", dind::WRAPPER_DIR, dind::SHIM_DIR),
            );
        }
    }
    Ok(())
}

/// Create and start the isolated job container, returning its id.
///
/// `needs_docker_sibling` forces this job's workspace isolation to copy-in
/// (regardless of the run's requested strategy) and binds a run-scoped named
/// volume at the workspace path instead of leaving it container-local, so a
/// Docker action's sibling container can mount the *same* volume
/// (`crate::executor::actions::docker_action` module docs) — `greenlit-init`
/// itself needs no change: its copy-in populate step fills whatever is
/// already bind-mounted at the workspace path, oblivious to whether that is
/// container-local storage or a named volume.
/// Everything that shapes the job container, gathered so the boot call keeps
/// one argument per *concern* rather than one per field.
struct BootRequest<'a> {
    /// The resolved image reference.
    image: &'a str,
    /// Whether this is a user-declared `container:` rather than a Greenlit
    /// runner image — the flag that gates every convergence behavior.
    in_container: bool,
    /// Job-container `env:`/`volumes:`/credentials, already validated.
    additions: &'a ContainerAdditions,
    /// Read-only binds the job's `uses:` steps need.
    extra_binds: &'a [BindMount],
    /// Whether a Docker action forces the shared-workspace volume.
    needs_docker_sibling: bool,
    /// The job's own bridge network.
    network: &'a str,
}

async fn boot_container(
    shared: &Shared<'_>,
    request: &BootRequest<'_>,
    progress: &mut (dyn ProgressSink + Send),
) -> Result<String, ExecError> {
    let BootRequest {
        image,
        in_container,
        additions,
        extra_binds,
        needs_docker_sibling,
        network,
    } = *request;
    let repo = shared.config.repo_host_path.to_string_lossy().into_owned();
    let idle = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("touch {READY_MARKER}; exec tail -f /dev/null"),
    ];
    let strategy = if needs_docker_sibling {
        IsolationStrategy::CopyIn
    } else {
        shared.config.strategy
    };
    let mut spec = isolation_container_spec(
        image.to_string(),
        &repo,
        &shared.config.workspace,
        strategy,
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
    if needs_docker_sibling {
        spec.binds.push(BindMount {
            host_path: namespaced_volume_name(
                &shared.config.volume_namespace,
                DOCKER_SIBLING_WORKSPACE_VOLUME,
            ),
            container_path: shared.config.workspace.clone(),
            read_only: false,
        });
    }
    spec.binds.extend(additions.volume_binds.iter().cloned());
    spec.binds.extend(extra_binds.iter().cloned());
    spec.env = additions.env.clone();
    spec.network = Some(network.to_string());
    // The persistent toolcache: a writable Greenlit-owned host directory, so a
    // `setup-*` action finds what a previous run installed instead of
    // downloading it again (`PHASE-4-environment.md`: "mount
    // `~/.litci/toolcache` at `RUNNER_TOOL_CACHE`"). A failure to create it
    // means no toolcache this run, not a failed run.
    if let Some(store) = shared.config.store.as_ref() {
        match services::toolcache_bind(
            &store.toolcache_root,
            &shared.config.runner_env.runner_tool_cache,
        ) {
            Ok(bind) => spec.binds.push(bind),
            Err(error) => tracing::debug!(
                target: "greenlit_runtime::services",
                %error,
                "could not prepare the toolcache; running without it"
            ),
        }
    }
    spec.labels = vec![("greenlit.managed".to_string(), "1".to_string())];

    let engine = shared.engine;
    progress.on_progress(ProgressEvent::BootStarted);
    let boot = async {
        let id = engine.create_container(&spec).await?;
        engine.start_container(&id).await?;
        Ok::<_, ExecError>(id)
    };
    let id = boot.instrument(stage_span("container-boot")).await?;
    progress.on_progress(ProgressEvent::BootFinished);
    Ok(id)
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

/// Resolves every `services:` entry against the job's contexts and validates
/// it exactly as a job container is validated.
///
/// A service is as untrusted as a job container: it goes through the same
/// option and volume gates, so `--privileged` on a service is refused for the
/// same reason it is refused on `container:`. Credentials are masked before
/// validation so a pull failure cannot echo them.
fn resolve_services(
    shared: &Shared<'_>,
    masker: &mut Masker,
    runner_ctx: &Value,
    instance: &JobInstance<'_>,
    base_env: &IndexMap<String, String>,
    needs: &[NeedRecord],
) -> Result<Vec<(String, services::ResolvedService)>, ExecError> {
    if instance.services.is_empty() {
        return Ok(Vec::new());
    }
    let ctx = env_ctx(
        shared.roots,
        runner_ctx,
        &instance.matrix,
        base_env,
        needs,
        needs_status(needs),
    );

    let mut resolved = Vec::new();
    for (id, plan) in instance.services {
        let request = resolve_container(plan, &ctx)?;
        if let Some(credentials) = &request.credentials {
            masker.add(&credentials.password);
        }
        let additions = validate_container(
            &request,
            &shared.config.workspace,
            &shared.config.volume_namespace,
        )
        .map_err(|rejection| ExecError::Infrastructure {
            message: format!("service `{id}`: {rejection}"),
            fix: "correct the service definition in the workflow".to_string(),
        })?;

        resolved.push((
            id.clone(),
            services::ResolvedService {
                image: request.image.clone(),
                env: additions.env,
                ports: additions.ports,
                volume_binds: additions.volume_binds,
                healthcheck: additions.healthcheck,
                registry_auth: additions.registry_auth,
            },
        ));
    }
    Ok(resolved)
}

/// A `run:` step's script text, as far as planning resolved it.
///
/// A script whose body is still deferred contributes its authored source, so
/// a `docker` call behind an expression is still detected — over-detecting
/// costs one container, under-detecting costs a confusing failure.
fn step_script(step: &greenlit_engine::StepPlan) -> Option<String> {
    match &step.kind {
        greenlit_engine::StepKind::Run { script, .. } => {
            let planned = script.as_ref();
            Some(match &planned.evaluation {
                greenlit_engine::Evaluation::Static(value) => value.clone(),
                // Still deferred: scan the authored source instead, so a
                // `docker` call behind an expression is not missed.
                greenlit_engine::Evaluation::Deferred(_) => planned.source.clone(),
            })
        }
        greenlit_engine::StepKind::Uses { .. } => None,
    }
}
