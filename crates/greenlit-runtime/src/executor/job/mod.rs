//! Running one job instance end to end: activation, image, container boot,
//! overlay readiness, the sequential step loop, job-output finalization, and
//! teardown.

mod boot;
mod helper_binary;
mod teardown;

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use indexmap::IndexMap;
use tracing::Instrument;

use greenlit_engine::execution::env::{EnvLayers, RunnerEnv, apply_path_additions, layer_step_env};
use greenlit_engine::execution::job_outputs::finalize_outputs;
use greenlit_engine::execution::outcome::{advance_status, job_result_from_status, needs_status};
use greenlit_engine::execution::resolve::{resolve_condition, resolve_string};
use greenlit_engine::execution::{Masker, NeedRecord};
use greenlit_engine::{Conclusion, ContainerPlan, JobId};
use greenlit_expr::{Context, RunStatus, Value};

use crate::engine::ExecSpec;
use crate::events::{ExecutionEvent, ExecutionEventSink, JobScope};
use crate::executor::actions::docker_action;
use crate::executor::actions::node_runtime;
use crate::executor::actions::nodejs::NodeRuntimeMounts;
use crate::executor::actions::resolve::{JobActionPlan, resolve_job_actions};
use crate::executor::cmdfiles;
use crate::executor::container::{ResolvedContainer, ResolvedCredentials, validate_container};
use crate::executor::context::{
    ContextRoots, LiveState, build_context, copy_event_env_vars, job_github_context,
    resolve_env_layer, runner_context,
};
use crate::executor::dind;
use crate::executor::event_json;
use crate::executor::instance::JobInstance;
use crate::executor::netguard;
use crate::executor::readiness;
use crate::executor::report::{JobReport, StepReport};
use crate::executor::services;
use crate::executor::step::{
    JobRuntime, StepLoopState, StepOutput, compute_step_action_ids, execute_step,
    mask_command_file_error, run_post_steps, run_pre_steps,
};
use crate::executor::{ExecError, Shared, stage_span};
use crate::progress::{ProgressEvent, ProgressSink};

pub(crate) use boot::{boot_container, seed_container_path};

/// Base directory for per-step command files inside a container.
const CMDFILES_BASE: &str = "/greenlit/cmdfiles";

/// Cap on the `PATH` read back from an untrusted job image.
///
/// Generous next to any real `PATH` (a stock Ubuntu image is well under 100
/// bytes), while bounding what a hostile image can make the host hold.
const MAX_PATH_BYTES: usize = 32 * 1024;

/// The bare (pre-namespacing) names of the run-scoped named volumes backing
/// a job's shared workspace and command files when it contains a Docker
/// action (`crate::executor::actions::docker_action` module docs) — each
/// namespaced the same way a workflow-authored `volumes:` entry is
/// (`crate::executor::container::namespaced_volume_name`).
const DOCKER_SIBLING_VOLUMES: docker_action::SiblingVolumes = docker_action::SiblingVolumes {
    workspace: "workspace",
    cmdfiles: "cmdfiles",
};

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
    docker_volumes: Option<docker_action::SiblingVolumes>,
}

#[derive(Clone, Copy)]
pub(crate) struct JobIdentity<'a> {
    pub(crate) id: &'a JobId,
    pub(crate) instance_key: &'a str,
    pub(crate) matrix_index: usize,
}

pub(crate) struct JobChannels<'a> {
    pub(crate) log: &'a mut (dyn Write + Send),
    pub(crate) events: &'a mut (dyn ExecutionEventSink + Send),
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
    identity: JobIdentity<'_>,
    needs: &[NeedRecord],
    channels: JobChannels<'_>,
    progress: &mut (dyn ProgressSink + Send),
) -> Result<JobReport, ExecError> {
    let started = Instant::now();
    if shared.cancellation.is_cancelled() {
        let display = masker.apply(&instance.display.source);
        channels.events.on_event(ExecutionEvent::JobFinished {
            scope: JobScope {
                job_id: identity.id.0.clone(),
                instance_id: identity.instance_key.to_string(),
                display: display.clone(),
            },
            conclusion: Conclusion::Cancelled,
            duration: started.elapsed(),
        });
        return Ok(cancelled_report(identity.id, display, started));
    }

    let mut runner_env = shared.config.runner_env.clone();
    runner_env.job = identity.id.0.clone();
    runner_env.workspace = shared.config.workspace.clone();

    // The job's own bridge: the shim binds on its gateway and, from the next
    // task group, services attach to it. Created before the container so the
    // container can be attached at creation time, and torn down after it --
    // a network with an attached container cannot be removed.
    let job_namespace = format!(
        "{}-{}",
        shared.config.volume_namespace, identity.instance_key
    );
    let network_name = format!("greenlit-run-{job_namespace}");
    let job_network =
        services::create(shared.engine, &network_name, shared.config.store.as_ref()).await?;
    runner_env.actions_service = job_network.actions_service().cloned();

    let mut base_env = runner_env.clone().into_map();
    // `GITHUB_WORKFLOW_REF`/`GITHUB_WORKFLOW_SHA`/`GITHUB_HEAD_REF`/
    // `GITHUB_BASE_REF`: copied straight off the run's already-assembled
    // `github` object rather than re-derived (`context::copy_event_env_vars`
    // doc comment) -- same value for every job in the run, so this can run
    // once here alongside the rest of `base_env`'s construction.
    copy_event_env_vars(&mut base_env, &shared.roots.github);
    // `GITHUB_EVENT_PATH`: the in-container path `write_event_file` below
    // writes the synthetic event payload to, once the container exists.
    // Every job's `CMDFILES_BASE` is the same in-container constant string
    // (`crate::executor::cmdfiles::event_file_path`'s doc comment), so this
    // value is already correct even though the file itself isn't written
    // until later in this function -- `base_env` is the seam every step kind
    // (`run:`, every `uses:` variant, composite nesting) ultimately layers
    // its own env on top of, matching how `GITHUB_WORKFLOW`/`GITHUB_SHA`/...
    // already reach every step from here.
    let event_path = cmdfiles::event_file_path(CMDFILES_BASE);
    base_env.insert("GITHUB_EVENT_PATH".to_string(), event_path.clone());
    let runner_ctx = runner_context(&runner_env);

    // The run-wide `github` context roots patched with this job instance's
    // own runtime-only properties (`context::job_github_context` doc
    // comment: `job`/`event_path`/`workspace`/`run_id`/`run_number`/
    // `run_attempt`, the classify.rs-deferred subset this crate can now
    // supply). `shared` is shadowed with a job-scoped copy for the rest of
    // this function so every downstream `shared.roots`/`env_ctx` call below
    // (activation, `env:`/`defaults:` resolution, service/container
    // resolution, the job body, job outputs) sees the patched object without
    // threading a second parameter through each of them.
    let job_roots = ContextRoots {
        github: job_github_context(&shared.roots.github, &runner_env, &event_path),
        vars: shared.roots.vars.clone(),
        inputs: shared.roots.inputs.clone(),
        secrets: shared.roots.secrets.clone(),
        fs: Arc::clone(&shared.roots.fs),
    };
    let job_shared = Shared {
        engine: shared.engine,
        config: shared.config,
        roots: &job_roots,
        workflow_env: shared.workflow_env,
        cancellation: shared.cancellation,
        namespace: &job_namespace,
    };
    let shared = &job_shared;

    // The run's `github.event` payload, serialized to real JSON once up
    // front (`crate::executor::event_json`) -- the same for every job in
    // the run, so this can be computed here and written after the container
    // is ready, below. `to_json`'s tree is built entirely from
    // `Value::{Null,Bool,Number,String,Array,Object}`, which
    // `serde_json::to_string` cannot fail to serialize (no user `Serialize`
    // impl in that tree can error), so the fallback here is unreachable in
    // practice rather than a silently swallowed real failure.
    let event_payload = match &shared.roots.github {
        Value::Object(object) => object.get("event").cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let event_json_text = serde_json::to_string_pretty(&event_json::to_json(&event_payload))
        .unwrap_or_else(|_| "null".to_string());

    let needs_run_status = needs_status(needs);
    let activation_ctx = env_ctx(
        shared.roots,
        &runner_ctx,
        &Value::Null,
        &base_env,
        needs,
        needs_run_status,
    );
    let display_ctx = env_ctx(
        shared.roots,
        &runner_ctx,
        &instance.matrix,
        &base_env,
        needs,
        needs_run_status,
    );
    // Masked before any print/report, same reasoning as a step's `name:`
    // (`crate::executor::step::execute_step`): a job's display name can
    // interpolate `${{ needs.<id>.outputs.* }}`, and an upstream job may have
    // masked that value via `::add-mask::`.
    let display =
        masker.apply(&resolve_string(instance.display, &display_ctx).map_err(ExecError::eval)?);
    let event_scope = JobScope {
        job_id: identity.id.0.clone(),
        instance_id: identity.instance_key.to_string(),
        display: display.clone(),
    };

    if !job_activates(instance, needs, &activation_ctx)? {
        channels.events.on_event(ExecutionEvent::JobSkipped {
            scope: event_scope,
            reason: "job condition evaluated false".to_string(),
        });
        return Ok(skipped_report(identity.id, display, started));
    }
    channels.events.on_event(ExecutionEvent::JobStarted {
        scope: event_scope.clone(),
    });
    let mut step_output = StepOutput {
        log: channels.log,
        events: channels.events,
        scope: event_scope,
        next_index: 0,
    };

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
    let action_plan = tokio::select! {
        result = resolve_job_actions(
            instance.steps,
            &shared.config.actions,
            &shared.config.repo_host_path,
            &shared.config.workspace,
        ) => match result {
            Ok(plan) => plan,
            Err(error) => {
                job_network.teardown(shared.engine).await;
                return Err(error);
            }
        },
        () = shared.cancellation.cancelled() => {
            job_network.teardown(shared.engine).await;
            return Ok(cancelled_report(identity.id, display, started));
        }
    };
    let (node_binds, node_mounts) = if action_plan.needs_node20 || action_plan.needs_node24 {
        progress.on_progress(ProgressEvent::ActionRuntimeEnsureStarted);
        let ensure = node_runtime::ensure_mounts(
            &shared.config.actions.node_runtime_store,
            shared.config.actions.node_runtime_fetcher.as_ref(),
            shared.config.actions.node_runtime_specs.as_ref(),
            action_plan.needs_node20,
            action_plan.needs_node24,
        )
        .instrument(stage_span("action-runtime-ensure"));
        let ensured = tokio::select! {
            result = ensure => match result {
                Ok(mounts) => mounts,
                Err(source) => {
                    job_network.teardown(shared.engine).await;
                    return Err(ExecError::Infrastructure {
                        message: format!("could not prepare a pinned Node action runtime: {source}"),
                        fix: "check network connectivity and retry".to_string(),
                    });
                }
            },
            () = shared.cancellation.cancelled() => {
                job_network.teardown(shared.engine).await;
                return Ok(cancelled_report(identity.id, display, started));
            }
        };
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
    let services_started = match resolve_services(
        shared,
        masker,
        &runner_ctx,
        instance,
        &base_env,
        needs,
        job_network.policy().gateway.as_deref(),
    ) {
        Ok(resolved) => {
            match services::start(
                shared.engine,
                &services::ServiceRuntime {
                    config: shared.config,
                    network: job_network.name(),
                    policy: job_network.policy(),
                    cancellation: shared.cancellation,
                },
                &resolved,
                masker,
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
                    if failure.cancelled {
                        return Ok(cancelled_report(identity.id, display, started));
                    }
                    return Err(failure.cause);
                }
            }
        }
        Err(error) => {
            job_network.teardown(shared.engine).await;
            return Err(error);
        }
    };

    let resolved_image = tokio::select! {
        result = boot::resolve_image(
            shared,
            masker,
            &runner_ctx,
            boot::ResolveImageRequest { instance, identity },
            &base_env,
            needs,
            progress,
        ) => match result {
            Ok(image) => image,
            Err(error) => {
                services::stop(shared.engine, &services_started).await;
                job_network.teardown(shared.engine).await;
                return Err(error);
            }
        },
        () = shared.cancellation.cancelled() => {
            services::stop(shared.engine, &services_started).await;
            job_network.teardown(shared.engine).await;
            return Ok(cancelled_report(identity.id, display, started));
        }
    };
    let boot::ResolvedImage {
        tag: image_tag,
        in_container,
        bash_available,
        additions,
    } = resolved_image;

    let container = boot_container(
        shared,
        &boot::BootRequest {
            image: &image_tag,
            in_container,
            additions: &additions,
            extra_binds: &extra_binds,
            needs_docker_sibling: action_plan.needs_docker_sibling,
            network: job_network.name(),
        },
        progress,
    )
    .await;
    let container = match container {
        Ok(Some(container)) => container,
        Ok(None) => {
            services::stop(shared.engine, &services_started).await;
            job_network.teardown(shared.engine).await;
            return Ok(cancelled_report(identity.id, display, started));
        }
        Err(error) => {
            services::stop(shared.engine, &services_started).await;
            job_network.teardown(shared.engine).await;
            return Err(error);
        }
    };

    // The network policy goes on before readiness, and therefore before any
    // workflow code runs: a container that has executed even one step under
    // an unrestricted network has already had its chance to reach the LAN.
    let netguard_image = shared.config.locked_image(netguard::NETGUARD_IMAGE)?;
    if shared.config.locked_images.is_some() && !shared.engine.image_exists(&netguard_image).await?
    {
        let _ = shared.engine.remove_container(&container).await;
        services::stop(shared.engine, &services_started).await;
        job_network.teardown(shared.engine).await;
        return Err(ExecError::Infrastructure {
            message: format!(
                "locked network-guard image '{netguard_image}' disappeared before job startup"
            ),
            fix: "retry to prepare the exact image again; do not prune Docker images during an active run"
                .to_string(),
        });
    }
    let netguard_result = tokio::select! {
        result = netguard::apply(
            shared.engine,
            &container,
            job_network.policy(),
            &netguard_image,
            shared.namespace,
            progress,
        ) => result.map_err(ExecError::from),
        () = shared.cancellation.cancelled() => Ok(()),
    };
    if shared.cancellation.is_cancelled() {
        let _ = shared.engine.remove_container(&container).await;
        services::stop(shared.engine, &services_started).await;
        job_network.teardown(shared.engine).await;
        return Ok(cancelled_report(identity.id, display, started));
    }
    if let Err(error) = netguard_result {
        let _ = shared.engine.remove_container(&container).await;
        services::stop(shared.engine, &services_started).await;
        job_network.teardown(shared.engine).await;
        return Err(error);
    }

    // Docker-in-Docker is an explicit privileged-infrastructure request.
    // Phase 12 never infers it from shell text: comments, `echo docker`, and
    // ordinary Docker clients are not authority to create a privileged
    // daemon. Runtime quarantine rejects this request until Phase 20
    // certifies a provisioning path.
    let dind = if shared.config.dind {
        let dind_start = tokio::select! {
            result = dind::start(
                shared.engine,
                job_network.name(),
                shared.namespace,
                progress,
            ) => Some(result),
            () = shared.cancellation.cancelled() => None,
        };
        match dind_start {
            None => {
                let _ = shared.engine.remove_container(&container).await;
                services::stop(shared.engine, &services_started).await;
                job_network.teardown(shared.engine).await;
                return Ok(cancelled_report(identity.id, display, started));
            }
            Some(Ok(sidecar)) => Some(sidecar),
            Some(Err(error)) => {
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

    // Readiness runs against the freshly booted container, before the step
    // loop; its error still flows through the teardown below rather than
    // returning early and leaking the container.
    let docker_volumes = action_plan
        .needs_docker_sibling
        .then_some(DOCKER_SIBLING_VOLUMES);
    let job_work = async {
        let ready = readiness::wait_for_ready(
            shared.engine,
            &container,
            &shared.config.readiness,
            progress,
        )
        .instrument(stage_span("overlay-setup"))
        .await;
        match ready {
            Ok(()) => match seed_container_path(shared.engine, &container, &mut base_env).await {
                Ok(()) => match cmdfiles::write_event_file(
                    shared.engine,
                    &container,
                    CMDFILES_BASE,
                    &event_json_text,
                )
                .await
                {
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
                                docker_volumes,
                            },
                            (&container, &runner_env),
                            needs,
                            &mut step_output,
                        )
                        .await
                    }
                    Err(error) => Err(mask_command_file_error(masker, &error)),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        }
    };
    let (outcome, cancelled) = tokio::select! {
        outcome = job_work => (outcome, false),
        () = shared.cancellation.cancelled() => (
            Ok((Vec::new(), IndexMap::new(), Conclusion::Cancelled)),
            true,
        ),
    };

    teardown::teardown(
        shared,
        &container,
        docker_volumes,
        dind.as_ref(),
        &services_started,
        job_network,
        shared.config.write_back && !cancelled,
    )
    .await;

    let (step_reports, outputs, result) = outcome?;
    let duration = started.elapsed();
    step_output.events.on_event(ExecutionEvent::JobFinished {
        scope: step_output.scope,
        conclusion: result,
        duration,
    });
    Ok(JobReport {
        id: identity.id.0.clone(),
        display,
        result,
        steps: step_reports,
        outputs,
        duration,
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
    output: &mut StepOutput<'_>,
) -> Result<(Vec<StepReport>, IndexMap<String, String>, Conclusion), ExecError> {
    // `GITHUB_ACTION` per step: a pure function of the authored plan (never
    // runtime data), so every step's value is settled once, up front, rather
    // than re-derived per phase (`crate::executor::step::compute_step_action_ids`
    // doc comment).
    let step_action_ids = compute_step_action_ids(instance.steps);
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
        volume_namespace: shared.namespace,
        locked_images: shared.config.locked_images.as_ref(),
        docker_volumes: layers.docker_volumes,
        step_action_ids: &step_action_ids,
    };

    let mut state = StepLoopState::new();
    // Front-loaded, job-wide: every top-level action's `pre:` script runs
    // before the job's first step, in action (job step) order
    // (`crate::executor::step::run_pre_steps` module docs).
    run_pre_steps(&job_rt, needs, instance.steps, &mut state, masker, output)
        .instrument(stage_span("exec"))
        .await?;

    let mut step_reports = Vec::with_capacity(instance.steps.len());
    for (index, step) in instance.steps.iter().enumerate() {
        let executed = execute_step(&job_rt, needs, step, index, &mut state, masker, output)
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
    let post_executed = run_post_steps(&job_rt, needs, &mut state, masker, output)
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

fn cancelled_report(job_id: &JobId, display: String, started: Instant) -> JobReport {
    JobReport {
        id: job_id.0.clone(),
        display,
        result: Conclusion::Cancelled,
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
    publish_address: Option<&str>,
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
        let mut additions =
            validate_container(&request, &shared.config.workspace, shared.namespace).map_err(
                |rejection| ExecError::Infrastructure {
                    message: format!("service `{id}`: {rejection}"),
                    fix: "correct the service definition in the workflow".to_string(),
                },
            )?;
        if let Some(address) = publish_address {
            for port in &mut additions.ports {
                port.host_ip = Some(address.to_string());
            }
        }

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
