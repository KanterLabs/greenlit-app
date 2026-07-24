//! Composite action execution: scoped `inputs`/`steps` contexts, blocked
//! `secrets`, nested `uses:`.
//!
//! # Scoping
//!
//! - **`inputs` context**: the composite's own declared inputs (default
//!   overridden by the outer step's `with:`), the same default/override
//!   resolution `super::nodejs::input_env` computes for a JS action's
//!   `INPUT_*` vars — reused here as a `Value` object instead of env vars,
//!   since a composite's nested `${{ }}` text reads `inputs.*` directly
//!   rather than an env var.
//! - **`secrets` context is blocked**: `docs/adrs/1144-composite-actions.md`
//!   (`actions/runner` v2.336.0, pinned release — see
//!   `super::node_runtime` module docs): "The secrets context will **not**
//!   be available to composite actions, users will need to pass these
//!   values in as an input." This module builds the composite's nested
//!   context with `secrets` set to an empty object, so a nested `${{
//!   secrets.X }}` resolves to empty exactly like an undeclared context
//!   field would, never the run's real secret value.
//! - **`steps` context is composite-local**: a nested step's `${{
//!   steps.<id>.outputs.* }}` sees only *this composite's own* nested steps,
//!   never the enclosing job's — `outputs.<id>.value` is evaluated against
//!   this same local `steps` context after every nested step runs, then
//!   folded into the *job's* `steps.<outer_id>.outputs` exactly like an
//!   ordinary step's outputs (`crate::executor::step`).
//! - **`env:` accumulation is job-wide, not composite-scoped**: unlike
//!   `steps`/`inputs`/`secrets`, GitHub does not sandbox `GITHUB_ENV`/
//!   `GITHUB_PATH` writes to the composite — a nested step's env/path
//!   mutations are visible to the rest of the job exactly as if they came
//!   from an ordinary step, and vice versa. This module therefore mutates
//!   the *same* accumulation the outer job step loop owns
//!   (`CompositeState::job_accumulated`/`job_path_additions`), rather than
//!   keeping a separate copy.
//!
//! # Scope decision: nested-action pre timing and Docker actions
//!
//! GitHub hoists every action's `pre:` script — including ones nested
//! inside a composite — to run before the job's very first step
//! (`super::nodejs` module docs). Reproducing that exactly for a *nested*
//! action would require resolving the composite's own `with:` (which can
//! depend on the outer job's live context) during the job-wide pre-pass,
//! before any step runs at all. This module instead runs a nested action's
//! `pre:` immediately before that nested action's own `main:`, at the
//! composite's position in the job's step order — a documented, deliberate
//! simplification; **post ordering is not affected** (every nested post
//! still pushes onto the same job-wide LIFO stack `super::post_chain`
//! drains at job end, matching GitHub exactly). A Docker action nested
//! inside a composite is out of scope for this wave (fails with a clear,
//! honest message rather than attempting a partial sibling-container setup
//! that would need the whole job's Docker-sibling apparatus threaded
//! through composite recursion); a job-level Docker action (directly under
//! `jobs.<id>.steps`) is fully supported (`super::docker_action`).

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use greenlit_actions::manifest::{ActionInput, CompositeStep};
use greenlit_engine::execution::Masker;
use greenlit_engine::execution::contexts::{NeedRecord, StepRecord, build_steps_context};
use greenlit_engine::execution::env::{EnvLayers, apply_path_additions, layer_step_env};
use greenlit_engine::execution::outcome::{StepExit, step_result_from_exit};
use greenlit_expr::{Context, RunStatus, Value};
use indexmap::IndexMap;

use crate::engine::{ContainerEngine, ExecSpec};
use crate::executor::ExecError;
use crate::executor::cmdfiles::{self, CommandFilePaths};
use crate::executor::context::ContextRoots;
use crate::executor::logsink::StepLogSink;

use super::node_runtime::{self, NodeVariant};
use super::nodejs::{self, NodeRuntimeMounts};
use super::post_chain::{NodePostEntry, PostAction, PostChain, PostEntry};
use super::resolve::{ResolvedComposite, ResolvedCompositeStep, ResolvedUses};
use super::template;

/// Everything composite (and its own nested composite) execution needs from
/// the enclosing job, independent of any one step.
pub(crate) struct CompositeEnv<'a> {
    pub engine: &'a dyn ContainerEngine,
    pub container: &'a str,
    pub roots: &'a ContextRoots,
    pub runner_ctx: &'a Value,
    pub matrix: &'a Value,
    pub needs: &'a [NeedRecord],
    pub status: RunStatus,
    pub cmdfiles_base: &'a str,
    pub workspace: &'a str,
    pub node_mounts: &'a NodeRuntimeMounts,
}

/// Mutable, cross-call state a composite's execution (and any nested
/// composite/action inside it) shares with the enclosing job.
pub(crate) struct CompositeState<'a> {
    pub masker: &'a mut Masker,
    pub out: &'a mut (dyn Write + Send),
    pub post_chain: &'a mut PostChain,
    pub node_variant: &'a mut Option<NodeVariant>,
    /// The job's `GITHUB_ENV` accumulation — mutated in place by a nested
    /// `run:`/action step exactly like an ordinary job step (see module
    /// docs: env accumulation is job-wide, not composite-scoped).
    pub job_accumulated: &'a mut IndexMap<String, String>,
    pub job_path_additions: &'a mut Vec<String>,
    /// Per-step-instance `STATE_` maps, keyed by a process-wide-unique
    /// state key handed out by the caller (`crate::executor::step`).
    pub action_state: &'a mut HashMap<usize, IndexMap<String, String>>,
}

/// The result of running one composite step: its evaluated `outputs.*`
/// (folded into the outer job's `steps.<id>.outputs`) and its rolled-up
/// conclusion (a composite step's own outcome follows its nested steps'
/// rolling status, the same rule a job's status follows across its steps).
pub(crate) struct CompositeOutcome {
    pub outputs: IndexMap<String, String>,
    pub exit: StepExit,
}

struct NestedStepResult {
    exit: StepExit,
    outputs: IndexMap<String, String>,
}

/// Executes a resolved composite action's nested steps.
///
/// `with` is the outer step's already-resolved `with:` (fully evaluated
/// against the *enclosing* scope by the caller — the job's own context for
/// a top-level composite step, or a further-enclosing composite's context
/// for a nested one). `outer_step_index` seeds a stable state-key namespace
/// for every nested action instance's `STATE_`/pre-post bookkeeping.
///
/// # Errors
/// Returns [`ExecError`] on any nested engine/evaluation failure, or if a
/// nested step references `actions/checkout` or a Docker action (both out
/// of scope nested inside a composite for this wave — see module docs).
pub(crate) async fn execute(
    env: &CompositeEnv<'_>,
    state: &mut CompositeState<'_>,
    resolved: &ResolvedComposite,
    with: &IndexMap<String, String>,
    outer_step_index: usize,
) -> Result<CompositeOutcome, ExecError> {
    let inputs_value = composite_inputs_value(&resolved.inputs, with)?;

    let mut composite_records: Vec<StepRecord> = Vec::new();
    let mut composite_status = RunStatus::Success;

    for (nested_index, step) in resolved.steps.iter().enumerate() {
        let state_key = outer_step_index * 1000 + nested_index;
        let ctx = build_composite_context(env, state, &inputs_value, &composite_records);

        let condition_true = match &step.source.if_condition {
            Some(raw) => {
                template::resolve_condition(raw, &ctx).map_err(ExecError::template_eval)?
            }
            None => true,
        };
        let activates = matches!(composite_status, RunStatus::Success) && condition_true;
        if !activates {
            if let Some(id) = &step.source.id {
                composite_records.push(StepRecord {
                    id: id.clone(),
                    outcome: greenlit_engine::Conclusion::Skipped,
                    conclusion: greenlit_engine::Conclusion::Skipped,
                    outputs: IndexMap::new(),
                });
            }
            continue;
        }

        let outcome =
            run_nested_step(env, state, step, &ctx, &resolved.action_path, state_key).await?;
        let continue_on_error = resolve_continue_on_error(&step.source, &ctx)?;
        let result = step_result_from_exit(outcome.exit, continue_on_error);
        if let Some(id) = &step.source.id {
            composite_records.push(StepRecord {
                id: id.clone(),
                outcome: result.outcome,
                conclusion: result.conclusion,
                outputs: outcome.outputs,
            });
        }
        composite_status = match result.conclusion {
            greenlit_engine::Conclusion::Failure => RunStatus::Failure,
            greenlit_engine::Conclusion::Cancelled => RunStatus::Cancelled,
            _ => composite_status,
        };
    }

    let final_ctx = build_composite_context(env, state, &inputs_value, &composite_records);
    let mut outputs = IndexMap::new();
    for (id, output) in &resolved.outputs {
        if let Some(value_expr) = &output.value {
            let value = template::resolve_template(value_expr, &final_ctx)
                .map_err(ExecError::template_eval)?;
            outputs.insert(id.clone(), value);
        }
    }
    let exit = match composite_status {
        RunStatus::Failure => StepExit::Failed,
        RunStatus::Cancelled => StepExit::Cancelled,
        _ => StepExit::Success,
    };
    Ok(CompositeOutcome { outputs, exit })
}

/// Builds a composite's `inputs` context value: every declared input's
/// default, overridden by the (already-resolved) `with:` map — the same
/// precedence `super::nodejs::input_env` applies, as a `Value` object
/// instead of `INPUT_*` env pairs.
fn composite_inputs_value(
    declared: &IndexMap<String, ActionInput>,
    with: &IndexMap<String, String>,
) -> Result<Value, ExecError> {
    let mut effective: IndexMap<String, String> = IndexMap::new();
    for (id, input) in declared {
        if let Some(default) = &input.default {
            effective.insert(id.clone(), default.clone());
        }
    }
    for (key, value) in with {
        effective.insert(key.clone(), value.clone());
    }
    Ok(Value::object(
        effective
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect(),
    ))
}

/// Runs one nested step: a plain `run:` shell command, or another resolved
/// action (recursing for a nested composite). `action_path` is the
/// enclosing composite's own root directory, exposed to a nested `run:` as
/// `GITHUB_ACTION_PATH` exactly as GitHub does for a composite's own steps.
async fn run_nested_step(
    env: &CompositeEnv<'_>,
    state: &mut CompositeState<'_>,
    step: &ResolvedCompositeStep,
    ctx: &Context,
    action_path: &str,
    state_key: usize,
) -> Result<NestedStepResult, ExecError> {
    if let Some(uses) = &step.uses {
        let reference = step.source.uses.as_deref().unwrap_or("nested action");
        return run_nested_uses(
            env,
            state,
            uses,
            reference,
            &step.source.with,
            ctx,
            state_key,
        )
        .await;
    }
    let Some(script_raw) = &step.source.run else {
        return Err(ExecError::Infrastructure {
            message: "composite step has neither `run` nor `uses`".to_string(),
            fix: "internal error: report this as a Greenlit defect (the manifest parser should reject this shape)".to_string(),
        });
    };
    let script = template::resolve_template(script_raw, ctx).map_err(ExecError::template_eval)?;
    let shell_raw = step.source.shell.as_deref().unwrap_or("bash");
    let shell_resolved =
        template::resolve_template(shell_raw, ctx).map_err(ExecError::template_eval)?;
    let cmdfiles_dir = nested_cmdfiles_dir(env.cmdfiles_base, state_key);
    let shell = greenlit_engine::execution::shell::resolve_shell(
        greenlit_engine::execution::shell::ShellSelection {
            step_shell: Some(&shell_resolved),
            default_shell: None,
            in_container: false,
            bash_available: true,
        },
        &format!("{cmdfiles_dir}/script"),
    )
    .map_err(|source| ExecError::Shell {
        label: "composite step".to_string(),
        source,
    })?;

    let mut step_env = IndexMap::new();
    for (name, value) in &step.source.env {
        step_env.insert(
            name.clone(),
            template::resolve_template(value, ctx).map_err(ExecError::template_eval)?,
        );
    }
    let empty = IndexMap::new();
    let mut full_env = layer_step_env(EnvLayers {
        base: &empty,
        workflow: &empty,
        job: state.job_accumulated,
        accumulated: &empty,
        step: &step_env,
    });
    apply_path_additions(&mut full_env, state.job_path_additions);

    let paths = CommandFilePaths::new(&cmdfiles_dir, 0);
    cmdfiles::prepare(env.engine, env.container, &paths, &script)
        .await
        .map_err(|source| ExecError::CommandFile(state.masker.apply(&source.to_string())))?;

    let working_dir = match &step.source.working_directory {
        Some(dir) => template::resolve_template(dir, ctx).map_err(ExecError::template_eval)?,
        // GitHub's default working directory for any step (including a
        // composite's own nested `run:`) is `GITHUB_WORKSPACE`.
        None => env.workspace.to_string(),
    };

    let mut exec_env: Vec<(String, String)> = full_env.into_iter().collect();
    exec_env.push(("GITHUB_ACTION_PATH".to_string(), action_path.to_string()));
    exec_env.push(("GITHUB_ENV".to_string(), paths.env.clone()));
    exec_env.push(("GITHUB_OUTPUT".to_string(), paths.output.clone()));
    exec_env.push(("GITHUB_PATH".to_string(), paths.path.clone()));
    exec_env.push(("GITHUB_STEP_SUMMARY".to_string(), paths.summary.clone()));

    let spec = ExecSpec {
        cmd: {
            let mut cmd = vec![shell.program];
            cmd.extend(shell.args);
            cmd
        },
        env: exec_env,
        working_dir: Some(working_dir),
    };
    let mut sink = StepLogSink::new(state.out, state.masker);
    let output = env.engine.exec(env.container, &spec, &mut sink).await?;
    sink.finish();
    let exit = if output.exit_code == 0 {
        StepExit::Success
    } else {
        StepExit::Failed
    };

    let effects = cmdfiles::collect(env.engine, env.container, &paths)
        .await
        .map_err(|source| ExecError::CommandFile(state.masker.apply(&source.to_string())))?;
    for assignment in &effects.env {
        state
            .job_accumulated
            .insert(assignment.name.clone(), assignment.value.clone());
    }
    if !effects.path_additions.is_empty() {
        let mut merged = effects.path_additions.clone();
        merged.append(state.job_path_additions);
        *state.job_path_additions = merged;
    }
    let mut outputs = IndexMap::new();
    for assignment in &effects.outputs {
        outputs.insert(assignment.name.clone(), assignment.value.clone());
    }
    Ok(NestedStepResult { exit, outputs })
}

/// Runs a nested `uses:` step's main phase (dispatching by kind), running
/// its `pre:` immediately beforehand and registering its `post:` onto the
/// shared job-wide chain — see module docs' "Scope decision". `ctx` is the
/// *enclosing* composite's context (its `inputs`/`steps`/blocked `secrets`),
/// against which `with_raw` (this nested step's own, still-raw `with:`
/// text) is evaluated.
async fn run_nested_uses(
    env: &CompositeEnv<'_>,
    state: &mut CompositeState<'_>,
    uses: &ResolvedUses,
    reference: &str,
    with_raw: &IndexMap<String, String>,
    ctx: &Context,
    state_key: usize,
) -> Result<NestedStepResult, ExecError> {
    match uses {
        ResolvedUses::Checkout => Err(ExecError::Infrastructure {
            message: "actions/checkout nested inside a composite action is not supported in v0"
                .to_string(),
            fix: "check out the repository as a top-level job step instead".to_string(),
        }),
        ResolvedUses::Docker(_) => Err(ExecError::Infrastructure {
            message: "a Docker action nested inside a composite action is not supported in v0"
                .to_string(),
            fix: "move this Docker action to a top-level job step".to_string(),
        }),
        ResolvedUses::Composite(nested) => {
            let with: IndexMap<String, String> = with_raw
                .iter()
                .map(|(k, v)| template::resolve_template(v, ctx).map(|value| (k.clone(), value)))
                .collect::<Result<_, crate::executor::actions::template::TemplateError>>()
                .map_err(ExecError::template_eval)?;
            let outcome = Box::pin(execute(env, state, nested, &with, state_key)).await?;
            Ok(NestedStepResult {
                exit: outcome.exit,
                outputs: outcome.outputs,
            })
        }
        ResolvedUses::Node(node) => {
            let with: IndexMap<String, String> = with_raw
                .iter()
                .map(|(k, v)| template::resolve_template(v, ctx).map(|value| (k.clone(), value)))
                .collect::<Result<_, crate::executor::actions::template::TemplateError>>()
                .map_err(ExecError::template_eval)?;
            let input_env = nodejs::input_env(reference, &node.inputs, &with, ctx)?;
            let variant =
                node_runtime::ensure_variant(env.engine, env.container, state.node_variant).await?;
            let node_binary = nodejs::node_binary(env.node_mounts, node.runs.using, variant)?;

            if let Some(pre) = &node.runs.pre {
                let pre_if = node.runs.pre_if.as_deref().unwrap_or("always()");
                if template::resolve_condition(pre_if, ctx).map_err(ExecError::template_eval)? {
                    nodejs::run_phase(
                        nodejs::PhaseRequest {
                            engine: env.engine,
                            container: env.container,
                            action_path: &node.action_path,
                            script: pre,
                            node_binary: &node_binary,
                            full_env: state.job_accumulated,
                            extra_env: &input_env,
                            cmdfiles_base: env.cmdfiles_base,
                            phase_key: &format!("{state_key}-pre"),
                            working_dir: env.workspace,
                        },
                        state.out,
                        state.masker,
                    )
                    .await?;
                }
            }
            let main_outcome = nodejs::run_phase(
                nodejs::PhaseRequest {
                    engine: env.engine,
                    container: env.container,
                    action_path: &node.action_path,
                    script: &node.runs.main,
                    node_binary: &node_binary,
                    full_env: state.job_accumulated,
                    extra_env: &input_env,
                    cmdfiles_base: env.cmdfiles_base,
                    phase_key: &format!("{state_key}-main"),
                    working_dir: env.workspace,
                },
                state.out,
                state.masker,
            )
            .await?;
            let state_map = state.action_state.entry(state_key).or_default();
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
                        with,
                        step_env: IndexMap::new(),
                        node_version: node.runs.using,
                        state_key,
                    })),
                });
            }
            let mut outputs = IndexMap::new();
            for assignment in &main_outcome.outputs {
                outputs.insert(assignment.name.clone(), assignment.value.clone());
            }
            Ok(NestedStepResult {
                exit: main_outcome.exit,
                outputs,
            })
        }
    }
}

fn resolve_continue_on_error(step: &CompositeStep, ctx: &Context) -> Result<bool, ExecError> {
    match &step.continue_on_error {
        Some(raw) => template::resolve_condition(raw, ctx).map_err(ExecError::template_eval),
        None => Ok(false),
    }
}

fn build_composite_context(
    env: &CompositeEnv<'_>,
    state: &CompositeState<'_>,
    inputs_value: &Value,
    composite_records: &[StepRecord],
) -> Context {
    Context::new(Arc::clone(&env.roots.fs))
        .with_github(env.roots.github.clone())
        .with_vars(env.roots.vars.clone())
        .with_inputs(inputs_value.clone())
        // Blocked per module docs' ADR citation: a composite never sees the
        // run's real secrets, only whatever it receives through `inputs`.
        .with_secrets(Value::object(vec![]))
        .with_runner(env.runner_ctx.clone())
        .with_matrix(env.matrix.clone())
        .with_env(env_value(state.job_accumulated))
        .with_steps(build_steps_context(composite_records))
        .with_needs(greenlit_engine::execution::build_needs_context(env.needs))
        .with_status(env.status)
}

fn env_value(env: &IndexMap<String, String>) -> Value {
    Value::object(
        env.iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    )
}

fn nested_cmdfiles_dir(base: &str, state_key: usize) -> String {
    format!("{base}/composite-{state_key}")
}
