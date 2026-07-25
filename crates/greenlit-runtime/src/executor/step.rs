//! Executing one step: finishing its plan-time-deferred values against the
//! live context, then either running a `run:` script as an exec, or
//! dispatching a `uses:` step to its resolved action kind
//! (`crate::executor::actions`) — folding either one's command files back
//! into the job's live state identically.
//!
//! Every GitHub-faithful decision here delegates to the engine's execution
//! semantics ([`greenlit_engine::execution`]): shell resolution, env layering,
//! the `outcome`/`conclusion` model, and the implicit `success()` gate. This
//! module is the runtime driver that turns those decisions into container execs.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use greenlit_engine::execution::env::{EnvLayers, RunnerEnv, apply_path_additions, layer_step_env};
use greenlit_engine::execution::outcome::{
    StepExit, StepResult, step_activates, step_result_from_exit, step_result_skipped,
};
use greenlit_engine::execution::resolve::{
    resolve_bool, resolve_condition, resolve_minutes, resolve_string,
};
use greenlit_engine::execution::shell::{ShellSelection, resolve_shell};
use greenlit_engine::execution::{Masker, NeedRecord, StepRecord};
use greenlit_engine::{Conclusion, StepKind, StepPlan};
use greenlit_expr::{Context, RunStatus, Value};

use crate::engine::{ContainerEngine, ExecSpec};
use crate::executor::ExecError;
use crate::executor::actions::node_runtime::NodeVariant;
use crate::executor::actions::post_chain::{NodePostEntry, PostAction, PostChain, PostEntry};
use crate::executor::actions::resolve::{JobActionPlan, ResolvedUses};
use crate::executor::actions::{checkout, composite, docker_action, node_runtime, nodejs};
use crate::executor::cmdfiles::{self, CommandFilePaths};
use crate::executor::context::{ContextRoots, LiveState, build_context, resolve_env_layer};
use crate::executor::logsink::StepLogSink;

// Re-exported so callers (`crate::executor::job`) reach it through this
// module alongside the rest of the per-job step-execution API, rather than
// naming the small `step_ids` module directly.
pub(crate) use crate::executor::step_ids::compute_step_action_ids;

/// Per-job constants a step needs — the container, the resolved env layers, the
/// job defaults, the run-wide context roots, and everything action execution
/// needs beyond that (resolved actions, action/node-runtime configuration,
/// the Docker-sibling shared volumes, if this job provisioned them).
pub(crate) struct JobRuntime<'a> {
    /// The container engine.
    pub engine: &'a dyn ContainerEngine,
    /// The running job container's id.
    pub container: &'a str,
    /// Run-wide context roots.
    pub roots: &'a ContextRoots,
    /// The `runner` context object.
    pub runner_ctx: &'a Value,
    /// The runner/`github` env template this instance resolved from
    /// (`crate::executor::actions::checkout` reads `repository`/`sha`/
    /// `ref_full` directly from it rather than round-tripping through
    /// expression `Value`s).
    pub runner_env: &'a RunnerEnv,
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
    /// This job's resolved `uses:` steps (`crate::executor::actions::resolve`).
    pub action_plan: &'a JobActionPlan,
    /// Action resolution/fetch/runtime configuration.
    pub action_config: &'a crate::executor::actions::ActionRuntimeConfig,
    /// Where this job's pinned Node runtimes were mounted, if any.
    pub node_mounts: &'a nodejs::NodeRuntimeMounts,
    /// This run's volume-namespacing token
    /// (`crate::RunConfig::volume_namespace`).
    pub volume_namespace: &'a str,
    /// Container aliases frozen by the RunLock.
    pub locked_images: Option<&'a std::collections::BTreeMap<String, String>>,
    /// The bare names of this job's shared Docker-action volumes, if
    /// [`JobActionPlan::needs_docker_sibling`] provisioned them
    /// (`crate::executor::actions::docker_action` module docs).
    pub docker_volumes: Option<docker_action::SiblingVolumes>,
    /// This job's precomputed per-step `GITHUB_ACTION` values, indexed the
    /// same as `JobInstance::steps` (`crate::executor::step_ids::compute_step_action_ids`).
    pub step_action_ids: &'a [String],
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
    /// The job-wide LIFO post-step stack every action instance (including
    /// ones nested inside a composite) registers onto
    /// (`crate::executor::actions::post_chain`).
    pub post_chain: PostChain,
    /// This job's cached libc probe result (`crate::executor::actions::node_runtime`),
    /// probed once and reused by every action in the job.
    pub node_variant: Option<NodeVariant>,
    /// Per-action-instance `STATE_` maps, keyed by a stable state key shared
    /// across that instance's pre/main/post phases.
    pub action_state: HashMap<usize, IndexMap<String, String>>,
}

impl StepLoopState {
    /// A fresh state for a job that is about to start.
    pub fn new() -> Self {
        StepLoopState {
            accumulated: IndexMap::new(),
            path_additions: Vec::new(),
            records: Vec::new(),
            status: RunStatus::Success,
            post_chain: PostChain::default(),
            node_variant: None,
            action_state: HashMap::new(),
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

/// Runs every top-level action's `pre:` script, in job order, before the
/// job's first step — `PHASE-3-actions.md`: "pre steps run before the first
/// step of the job in action order" (verified against the real runner,
/// `crate::executor::actions::nodejs` module docs). Each `pre-if` defaults to
/// `always()`, evaluated against the job's context *before any step has
/// run* (empty `steps` context, no `GITHUB_ENV` accumulation yet) — matching
/// the real runner's own front-loaded timing (a pre script genuinely cannot
/// see a later step's output, on GitHub either).
///
/// # Errors
/// Returns an [`ExecError`] on any engine or evaluation failure.
pub(crate) async fn run_pre_steps(
    job: &JobRuntime<'_>,
    needs: &[NeedRecord],
    steps: &[StepPlan],
    state: &mut StepLoopState,
    masker: &mut Masker,
    out: &mut (dyn Write + Send),
) -> Result<(), ExecError> {
    for (index, step) in steps.iter().enumerate() {
        let Some(ResolvedUses::Node(node)) =
            job.action_plan.per_step.get(index).and_then(Option::as_ref)
        else {
            continue;
        };
        let Some(pre_script) = &node.runs.pre else {
            continue;
        };
        let StepKind::Uses { with, .. } = &step.kind else {
            continue;
        };
        let ctx = pre_pass_context(job, needs, state);
        let with_resolved = resolve_env_layer(with, &ctx).map_err(ExecError::eval)?;
        let pre_if = node.runs.pre_if.as_deref().unwrap_or("always()");
        if !crate::executor::actions::template::resolve_condition(pre_if, &ctx)
            .map_err(ExecError::template_eval)?
        {
            continue;
        }
        let input_env =
            nodejs::input_env(&step_reference(step), &node.inputs, &with_resolved, &ctx)?;
        let variant =
            node_runtime::ensure_variant(job.engine, job.container, &mut state.node_variant)
                .await?;
        let node_binary = nodejs::node_binary(job.node_mounts, node.runs.using, variant)?;
        let mut full_env = layered_env(job, state, &IndexMap::new());
        // `GITHUB_ACTION`: this step's precomputed id (`job.step_action_ids`
        // doc comment) — a pre phase reports the same value its step's main
        // phase will, exactly like the real runner's per-step
        // `ExecutionContext` does across pre/main/post.
        full_env.insert(
            "GITHUB_ACTION".to_string(),
            job.step_action_ids[index].clone(),
        );
        let _ = writeln!(out, "  \u{25b6} Pre {}", step_reference(step));
        nodejs::run_phase(
            nodejs::PhaseRequest {
                engine: job.engine,
                container: job.container,
                action_path: &node.action_path,
                script: pre_script,
                node_binary: &node_binary,
                full_env: &full_env,
                extra_env: &input_env,
                cmdfiles_base: job.cmdfiles_base,
                phase_key: &format!("{index}-pre"),
                working_dir: job.workspace,
            },
            out,
            masker,
        )
        .await?;
    }
    Ok(())
}

/// Drains the job's post-step stack in LIFO order, running each entry
/// regardless of the job's own final status (`PHASE-3-actions.md`: "post
/// steps run in REVERSE order at job end REGARDLESS of failure") and
/// returning one [`ExecutedStep`] per entry, in drain order.
///
/// Known gap: a post phase does not currently get `GITHUB_ACTION` set
/// ([`run_node_post`]). `PostEntry::state_key` is a top-level step's own
/// plan index for a top-level action, but `outer*1000+nested` for one
/// nested inside a composite (`crate::executor::actions::composite`'s
/// `state_key`) — the two numberings alias whenever the composite is a
/// job's first step, so recovering "which step" from `state_key` alone
/// isn't reliable without a change to that module, which is out of scope
/// this change. Left absent rather than risk a wrong value for the aliased
/// case (this task's own "absent, not invented" standard for every other
/// runtime-unknown value).
///
/// # Errors
/// Returns an [`ExecError`] on any engine or evaluation failure.
pub(crate) async fn run_post_steps(
    job: &JobRuntime<'_>,
    needs: &[NeedRecord],
    state: &mut StepLoopState,
    masker: &mut Masker,
    out: &mut (dyn Write + Send),
) -> Result<Vec<ExecutedStep>, ExecError> {
    let entries = state.post_chain.drain_reverse();
    let mut results = Vec::with_capacity(entries.len());
    for entry in entries {
        let started = Instant::now();
        let label = masker.apply(&entry.label);
        let ctx = pre_pass_context(job, needs, state);
        let ran = match &entry.action {
            PostAction::Checkout(post) => {
                let _ = writeln!(out, "\u{25b6} {label}");
                checkout::run_post(job.engine, job.container, post).await?;
                true
            }
            PostAction::Node(post) => {
                let post_if = post.post_if.as_deref().unwrap_or("always()");
                if crate::executor::actions::template::resolve_condition(post_if, &ctx)
                    .map_err(ExecError::template_eval)?
                {
                    let _ = writeln!(out, "\u{25b6} {label}");
                    run_node_post(job, state, post, out, masker).await?;
                    true
                } else {
                    false
                }
            }
        };
        let duration = started.elapsed();
        if ran {
            let _ = writeln!(out, "  {} {label}", status_sigil(Conclusion::Success));
        } else {
            let _ = writeln!(out, "  \u{2013} {label} (skipped)");
        }
        results.push(ExecutedStep {
            result: if ran {
                StepResult {
                    outcome: Conclusion::Success,
                    conclusion: Conclusion::Success,
                }
            } else {
                step_result_skipped()
            },
            label,
            duration,
            ran,
        });
    }
    Ok(results)
}

async fn run_node_post(
    job: &JobRuntime<'_>,
    state: &mut StepLoopState,
    post: &NodePostEntry,
    out: &mut (dyn Write + Send),
    masker: &mut Masker,
) -> Result<(), ExecError> {
    let variant =
        node_runtime::ensure_variant(job.engine, job.container, &mut state.node_variant).await?;
    let node_binary = nodejs::node_binary(job.node_mounts, post.node_version, variant)?;
    let mut input_env: Vec<(String, String)> = post
        .with
        .iter()
        .map(|(k, v)| {
            (
                format!("INPUT_{}", k.replace(' ', "_").to_uppercase()),
                v.clone(),
            )
        })
        .collect();
    if let Some(state_map) = state.action_state.get(&post.state_key) {
        for (name, value) in state_map {
            input_env.push((format!("STATE_{name}"), value.clone()));
        }
    }
    let full_env = layered_env(job, state, &post.step_env);
    nodejs::run_phase(
        nodejs::PhaseRequest {
            engine: job.engine,
            container: job.container,
            action_path: &post.action_path,
            script: &post.post_script,
            node_binary: &node_binary,
            full_env: &full_env,
            extra_env: &input_env,
            cmdfiles_base: job.cmdfiles_base,
            phase_key: &format!("{}-post", post.state_key),
            working_dir: job.workspace,
        },
        out,
        masker,
    )
    .await?;
    Ok(())
}

/// Execute step `index`, mutating the job's live state and streaming its log.
///
/// # Errors
///
/// Returns [`ExecError`] on an engine failure, an unfinished expression, an
/// unsupported shell, an action resolution/execution failure, or a malformed
/// command file.
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

    // The step's own `env:` layer must be resolved and folded in *before* its
    // `name:` and `if:` are evaluated: GitHub exposes a step's own `env:` to
    // its own `if:` condition (the env context reflects the step's declared
    // env before the condition gates it) — see
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions
    // ("env context") and the accompanying job-level `if:`/`env:` ordering
    // notes. Resolving it here (rather than after the skip check) also lets a
    // step's `name:` reference its own `env:`.
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

    // Mask immediately, before the label is printed anywhere or stored into
    // the returned report: a `name:` can interpolate a prior step's output
    // (e.g. `${{ steps.one.outputs.credential }}`), and that output may carry
    // a value an earlier `::add-mask::`/masked secret registered. Every later
    // use of `label` — the live skip/start/result lines below, and the
    // `StepReport`/metrics record the caller builds from `ExecutedStep` — must
    // see the redacted form, matching the "secret values are masked in all
    // log output" security invariant (`AGENTS.md`).
    let label = masker.apply(&step_label(step, index, &ctx)?);

    let condition_true = match &step.condition {
        Some(condition) => resolve_condition(condition, &ctx).map_err(ExecError::eval)?,
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

    let continue_on_error = match &step.continue_on_error {
        Some(planned) => resolve_bool(planned, &ctx).map_err(ExecError::eval)?,
        None => false,
    };

    let started = Instant::now();
    let mut io = StepIo { out, masker };
    let (exit, outputs) = match &step.kind {
        StepKind::Run { .. } => {
            let _ = writeln!(io.out, "\u{25b6} {label}");
            run_script_step(job, step, &ctx, &full_env, index, state, &mut io).await?
        }
        StepKind::Uses { with, .. } => {
            let with_resolved = resolve_env_layer(with, &ctx).map_err(ExecError::eval)?;
            let _ = writeln!(io.out, "\u{25b6} {label}");
            execute_uses_step(job, needs, step, index, &with_resolved, state, &mut io).await?
        }
    };
    let duration = started.elapsed();

    let result = step_result_from_exit(exit, continue_on_error);
    if let Some(id) = &step.id {
        state.records.push(StepRecord {
            id: id.clone(),
            outcome: result.outcome,
            conclusion: result.conclusion,
            outputs,
        });
    }
    let _ = writeln!(io.out, "  {} {label}", status_sigil(result.conclusion));
    Ok(ExecutedStep {
        result,
        label,
        duration,
        ran: true,
    })
}

/// Bundles the step loop's two output sinks (the live log and the running
/// masker) so a step-execution helper takes one parameter for them instead
/// of two — the one pairing every dispatch path ([`run_script_step`],
/// [`execute_uses_step`]) needs identically.
struct StepIo<'a> {
    out: &'a mut (dyn Write + Send),
    masker: &'a mut Masker,
}

/// Runs a `run:` step's script exec — the Phase 2 path, unchanged in
/// behavior; factored out so [`execute_step`] can share one tail
/// (outcome/label/record-keeping) between it and [`execute_uses_step`].
async fn run_script_step(
    job: &JobRuntime<'_>,
    step: &StepPlan,
    ctx: &Context,
    full_env: &IndexMap<String, String>,
    index: usize,
    state: &mut StepLoopState,
    io: &mut StepIo<'_>,
) -> Result<(StepExit, IndexMap<String, String>), ExecError> {
    let StepKind::Run {
        script: script_planned,
        shell: step_shell_planned,
    } = &step.kind
    else {
        return Err(ExecError::Infrastructure {
            message: "internal error: run_script_step called on a non-run: step".to_string(),
            fix: "report this as a Greenlit defect".to_string(),
        });
    };
    let label_for_errors = step_label(step, index, ctx)?;
    let paths = CommandFilePaths::new(job.cmdfiles_base, index);
    let step_shell = match step_shell_planned {
        Some(planned) => Some(resolve_string(planned, ctx).map_err(ExecError::eval)?),
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
        label: label_for_errors.clone(),
        source,
    })?;
    let script = resolve_string(script_planned, ctx).map_err(ExecError::eval)?;
    let working_dir = resolve_working_dir(step, job, ctx)?;
    let timeout = match &step.timeout_minutes {
        Some(planned) => {
            Some(
                resolve_minutes(planned, ctx).map_err(|source| ExecError::Timeout {
                    label: label_for_errors.clone(),
                    source,
                })?,
            )
        }
        None => None,
    };

    cmdfiles::prepare(job.engine, job.container, &paths, &script)
        .await
        .map_err(|source| mask_command_file_error(io.masker, &source))?;

    // `GITHUB_ACTION`: this step's precomputed id
    // (`crate::executor::step_ids` module docs).
    let exec_env = exec_env_vec(full_env, &paths, &job.step_action_ids[index]);
    let wrapped_cmd = {
        let mut cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("echo $$ > {} && exec \"$0\" \"$@\"", paths.pid),
        ];
        cmd.push(shell.program);
        cmd.extend(shell.args);
        cmd
    };
    let spec = ExecSpec {
        cmd: wrapped_cmd,
        env: exec_env,
        working_dir: Some(working_dir),
    };

    let exit = run_exec(
        job.engine,
        job.container,
        &spec,
        &paths.pid,
        timeout,
        io.out,
        io.masker,
    )
    .await?;

    let effects = cmdfiles::collect(job.engine, job.container, &paths)
        .await
        .map_err(|source| mask_command_file_error(io.masker, &source))?;
    let outputs =
        cmdfiles::apply_effects(&effects, &mut state.accumulated, &mut state.path_additions);
    if !effects.summary_within_limit {
        // GitHub drops an over-cap job summary and notes it; mirror that.
        let _ = writeln!(
            io.out,
            "  [warning] {label_for_errors}: GITHUB_STEP_SUMMARY exceeded the 1 MiB limit and was dropped"
        );
    }
    Ok((exit, outputs))
}

/// Build the exec environment vector, layering in the four command-file
/// paths and this step's `GITHUB_ACTION` (`crate::executor::step_ids`
/// module docs).
fn exec_env_vec(
    full_env: &IndexMap<String, String>,
    paths: &CommandFilePaths,
    action_id: &str,
) -> Vec<(String, String)> {
    let mut env = full_env.clone();
    env.insert("GITHUB_ACTION".to_string(), action_id.to_string());
    env.insert("GITHUB_ENV".to_string(), paths.env.clone());
    env.insert("GITHUB_OUTPUT".to_string(), paths.output.clone());
    env.insert("GITHUB_PATH".to_string(), paths.path.clone());
    env.insert("GITHUB_STEP_SUMMARY".to_string(), paths.summary.clone());
    env.into_iter().collect()
}

/// Dispatches a `uses:` step's main phase to its resolved action kind.
async fn execute_uses_step(
    job: &JobRuntime<'_>,
    needs: &[NeedRecord],
    step: &StepPlan,
    index: usize,
    with: &IndexMap<String, String>,
    state: &mut StepLoopState,
    io: &mut StepIo<'_>,
) -> Result<(StepExit, IndexMap<String, String>), ExecError> {
    let reference = step_reference(step);
    let Some(resolved) = job.action_plan.per_step.get(index).and_then(Option::as_ref) else {
        return Err(ExecError::Infrastructure {
            message: format!("'{reference}' has no resolved action (internal inconsistency)"),
            fix: "internal error: report this as a Greenlit defect".to_string(),
        });
    };

    match resolved {
        ResolvedUses::Checkout => {
            let step_span = uses_span(step);
            let outcome = checkout::execute(
                checkout::CheckoutRequest {
                    engine: job.engine,
                    container: job.container,
                    step_span: &step_span,
                    with,
                    runner_env: job.runner_env,
                    workspace: job.workspace,
                    github_token: job.action_config.github_token.as_deref(),
                },
                io.masker,
            )
            .await?;
            state.post_chain.push(PostEntry {
                label: format!("Post {reference}"),
                action: PostAction::Checkout(outcome.post),
            });
            Ok((StepExit::Success, outcome.outputs))
        }
        ResolvedUses::Node(node) => {
            let mut full_env = layered_env(job, state, &IndexMap::new());
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
            let input_env = nodejs::input_env(&reference, &node.inputs, with, &ctx)?;
            let variant =
                node_runtime::ensure_variant(job.engine, job.container, &mut state.node_variant)
                    .await?;
            let node_binary = nodejs::node_binary(job.node_mounts, node.runs.using, variant)?;
            // `GITHUB_ACTION` (`crate::executor::step_ids` module docs);
            // added after `ctx` above so it does not also leak into the
            // `env` context this step's own `${{ }}` text sees.
            full_env.insert(
                "GITHUB_ACTION".to_string(),
                job.step_action_ids[index].clone(),
            );
            let main_outcome = nodejs::run_phase(
                nodejs::PhaseRequest {
                    engine: job.engine,
                    container: job.container,
                    action_path: &node.action_path,
                    script: &node.runs.main,
                    node_binary: &node_binary,
                    full_env: &full_env,
                    extra_env: &input_env,
                    cmdfiles_base: job.cmdfiles_base,
                    phase_key: &format!("{index}-main"),
                    working_dir: job.workspace,
                },
                io.out,
                io.masker,
            )
            .await?;
            let outputs = cmdfiles::apply_effects(
                &main_outcome.effects,
                &mut state.accumulated,
                &mut state.path_additions,
            );
            if !main_outcome.effects.summary_within_limit {
                // GitHub drops an over-cap job summary and notes it; mirror that.
                let _ = writeln!(
                    io.out,
                    "  [warning] {reference}: GITHUB_STEP_SUMMARY exceeded the 1 MiB limit and was dropped"
                );
            }
            let state_map = state.action_state.entry(index).or_default();
            for assignment in &main_outcome.state {
                state_map.insert(assignment.name.clone(), assignment.value.clone());
            }
            if node.runs.post.is_some() {
                state.post_chain.push(PostEntry {
                    label: format!("Post {reference}"),
                    action: PostAction::Node(Box::new(NodePostEntry {
                        action_path: node.action_path.clone(),
                        post_script: node.runs.post.clone().unwrap_or_default(),
                        post_if: node.runs.post_if.clone(),
                        with: with.clone(),
                        step_env: IndexMap::new(),
                        node_version: node.runs.using,
                        state_key: index,
                    })),
                });
            }
            Ok((main_outcome.exit, outputs))
        }
        ResolvedUses::Composite(resolved_composite) => {
            // Composite nuance (citation-verified against the pinned
            // runner, `actions/runner` v2.336.0:
            // `CompositeActionHandler` never re-sets `github.action`/
            // `GITHUB_ACTION` for a nested step — every step nested inside
            // this composite should report *this* step's own `action_id`
            // (`crate::executor::step_ids`), not generate one of its own.
            // Propagating `action_id` into `composite::CompositeEnv` so its
            // nested execution can apply it is not done by this change —
            // `crate::executor::actions::composite` is owned by a
            // concurrently in-flight change this session must not touch —
            // so a nested step's `GITHUB_ACTION` is unset for now (a
            // pre-existing gap, not a regression this change introduces).
            let step_span = uses_span(step);
            let env = composite::CompositeEnv {
                engine: job.engine,
                container: job.container,
                roots: job.roots,
                runner_ctx: job.runner_ctx,
                matrix: job.matrix,
                needs,
                status: state.status,
                cmdfiles_base: job.cmdfiles_base,
                workspace: job.workspace,
                node_mounts: job.node_mounts,
                runner_env: job.runner_env,
                github_token: job.action_config.github_token.as_deref(),
                step_span: &step_span,
                volume_namespace: job.volume_namespace,
                locked_images: job.locked_images,
                docker_volumes: job.docker_volumes,
                base_env: job.base_env,
                workflow_env: job.workflow_env,
                job_env: job.job_env,
            };
            let mut composite_state = composite::CompositeState {
                masker: io.masker,
                out: io.out,
                post_chain: &mut state.post_chain,
                node_variant: &mut state.node_variant,
                job_accumulated: &mut state.accumulated,
                job_path_additions: &mut state.path_additions,
                action_state: &mut state.action_state,
            };
            let outcome =
                composite::execute(&env, &mut composite_state, resolved_composite, with, index)
                    .await?;
            Ok((outcome.exit, outcome.outputs))
        }
        ResolvedUses::Docker(resolved_docker) => {
            let Some(volumes) = job.docker_volumes else {
                return Err(ExecError::Infrastructure {
                    message: format!(
                        "'{reference}' is a Docker action, but this job's shared volumes were not provisioned"
                    ),
                    fix: "internal error: report this as a Greenlit defect".to_string(),
                });
            };
            let mut full_env = layered_env(job, state, &IndexMap::new());
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
            // `GITHUB_ACTION` (`crate::executor::step_ids` module docs);
            // added after `ctx` above, same reasoning as the Node arm.
            full_env.insert(
                "GITHUB_ACTION".to_string(),
                job.step_action_ids[index].clone(),
            );
            let paths = CommandFilePaths::new(job.cmdfiles_base, index);
            let outcome = docker_action::run_step(
                docker_action::DockerActionRequest {
                    engine: job.engine,
                    container: job.container,
                    resolved: resolved_docker,
                    reference: &reference,
                    with,
                    full_env: &full_env,
                    ctx: &ctx,
                    workspace: job.workspace,
                    cmdfiles_root: job.cmdfiles_base,
                    cmdfiles: &paths,
                    volumes,
                    volume_namespace: job.volume_namespace,
                    locked_images: job.locked_images,
                },
                io.out,
                io.masker,
            )
            .await?;
            let outputs = cmdfiles::apply_effects(
                &outcome.effects,
                &mut state.accumulated,
                &mut state.path_additions,
            );
            if !outcome.effects.summary_within_limit {
                // GitHub drops an over-cap job summary and notes it; mirror that.
                let _ = writeln!(
                    io.out,
                    "  [warning] {reference}: GITHUB_STEP_SUMMARY exceeded the 1 MiB limit and was dropped"
                );
            }
            Ok((outcome.exit, outputs))
        }
    }
}

/// Builds the job's fully layered `env` (base+workflow+job+accumulated+
/// `extra_step_env`), the same shape [`execute_step`] builds for a `run:`
/// step, for use by an action dispatch that needs it directly rather than
/// through a already-built [`Context`].
fn layered_env(
    job: &JobRuntime<'_>,
    state: &StepLoopState,
    extra_step_env: &IndexMap<String, String>,
) -> IndexMap<String, String> {
    let mut env = layer_step_env(EnvLayers {
        base: job.base_env,
        workflow: job.workflow_env,
        job: job.job_env,
        accumulated: &state.accumulated,
        step: extra_step_env,
    });
    apply_path_additions(&mut env, &state.path_additions);
    env
}

/// The job's context as it stands before any step has run (empty `steps`
/// context, no `GITHUB_ENV` accumulation) — used for the front-loaded pre
/// phase and, functionally identically, for post-if evaluation (which also
/// has no per-step `steps` entry of its own to add).
fn pre_pass_context(job: &JobRuntime<'_>, needs: &[NeedRecord], state: &StepLoopState) -> Context {
    let env = layered_env(job, state, &IndexMap::new());
    build_context(
        job.roots,
        &LiveState {
            runner: job.runner_ctx,
            matrix: job.matrix,
            env: &env,
            steps: &state.records,
            needs,
            status: state.status,
        },
    )
}

/// A step's `uses:` reference text, or a placeholder if this is somehow not
/// a `uses:` step (defensive; every caller already matched on `StepKind::Uses`).
fn step_reference(step: &StepPlan) -> String {
    match &step.kind {
        StepKind::Uses { reference, .. } => reference.clone(),
        StepKind::Run { .. } => "run step".to_string(),
    }
}

/// A step's authored span, for an action-execution error that needs to
/// point at the `uses:` value.
fn uses_span(step: &StepPlan) -> greenlit_workflow::Span {
    match &step.kind {
        StepKind::Uses { span, .. } => span.clone(),
        StepKind::Run { .. } => greenlit_workflow::Span::new(
            std::sync::Arc::from(""),
            greenlit_workflow::Location::new(1, 1),
            greenlit_workflow::Location::new(1, 1),
        ),
    }
}

/// Build the masked [`ExecError::CommandFile`] for a command-file failure.
///
/// `CommandFileError::InvalidLine` embeds the offending line verbatim, which
/// may itself be (or contain) a value an earlier `::add-mask::` registered —
/// e.g. a step writes a bare masked token to `GITHUB_OUTPUT` without `=`. The
/// error is masked here, immediately, rather than left to whichever consumer
/// eventually prints it (this crate's caller may not even hold the masker).
///
/// `pub(crate)`: also used by `crate::executor::job::run_instance` for its
/// own one-shot `write_event_file` call, which needs the identical masking
/// treatment for the same reason.
pub(crate) fn mask_command_file_error(
    masker: &Masker,
    source: &crate::executor::cmdfiles::CommandFileError,
) -> ExecError {
    ExecError::CommandFile(masker.apply(&source.to_string()))
}

/// Run the step exec, streaming output through a [`StepLogSink`], honoring an
/// optional per-step timeout.
async fn run_exec(
    engine: &dyn ContainerEngine,
    container: &str,
    spec: &ExecSpec,
    pid_file: &str,
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
                // A timed-out step is a failure (GitHub kills it). Dropping
                // the future here only stops *streaming* it — the process
                // itself keeps running inside the container unless we
                // explicitly terminate it, which would otherwise let its
                // background writes race a later step
                // (`ContainerEngine::terminate`'s doc comment has the full
                // rationale). Terminate before reporting the timeout so the
                // container is in a known state by the time the next step
                // starts.
                Err(_elapsed) => {
                    engine.terminate(container, pid_file).await?;
                    StepExit::TimedOut
                }
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
