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
//! # Nested action kinds
//!
//! Every kind a top-level `uses:` step can be, a nested one can be too:
//! JavaScript (`super::nodejs`), another composite (this module,
//! recursively), `actions/checkout` (`super::checkout`), and a Docker
//! action (`super::docker_action`). Each nested step runs through the *same*
//! execution module its top-level counterpart does rather than a parallel
//! implementation — a nested checkout registers its post entry on the same
//! job-wide chain, and a nested Docker action mounts the same run-scoped
//! volumes, because [`super::resolve::JobActionPlan::needs_docker_sibling`]
//! already accounts for Docker actions *at any nesting depth* when the job
//! container boots.
//!
//! # Scope decision: nested-action pre timing
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
//! drains at job end, matching GitHub exactly).

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use greenlit_actions::manifest::{ActionInput, CompositeStep};
use greenlit_engine::execution::Masker;
use greenlit_engine::execution::contexts::{NeedRecord, StepRecord, build_steps_context};
use greenlit_engine::execution::env::{EnvLayers, RunnerEnv, apply_path_additions, layer_step_env};
use greenlit_engine::execution::outcome::{
    Conclusion, StepExit, advance_status, step_activates, step_result_from_exit,
};
use greenlit_expr::{Context, RunStatus, Value, expr_calls_status_function};
use indexmap::IndexMap;

use crate::engine::{ContainerEngine, ExecSpec};
use crate::executor::ExecError;
use crate::executor::cmdfiles::{self, CommandFilePaths};
use crate::executor::context::{ContextRoots, LiveState, build_context};
use crate::executor::logsink::StepLogSink;

use super::node_runtime::{self, NodeVariant};
use super::nodejs::{self, NodeRuntimeMounts};
use super::post_chain::{NodePostEntry, PostAction, PostChain, PostEntry};
use super::resolve::{ResolvedComposite, ResolvedCompositeStep, ResolvedUses};
use super::{checkout, docker_action, template};

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
    /// The runner/`github` env this instance resolved from, which a nested
    /// `actions/checkout` reads `repository`/`sha`/`ref_full` from exactly
    /// as a top-level one does (`super::checkout`).
    pub runner_env: &'a RunnerEnv,
    /// The run's configured token, for a nested checkout of a *different*
    /// repository (a self-checkout needs none).
    pub github_token: Option<&'a str>,
    /// The enclosing workflow step's own `uses:` span. A `CompositeStep`
    /// carries no span of its own in this crate's model (`super::resolve`),
    /// so an error raised by a nested step points at the workflow text the
    /// author can actually edit — the step that referenced this composite.
    pub step_span: &'a greenlit_workflow::Span,
    /// This run's volume-namespacing token, for a nested Docker action's
    /// sibling container.
    pub volume_namespace: &'a str,
    /// Container aliases frozen by the RunLock.
    pub locked_images: Option<&'a std::collections::BTreeMap<String, String>>,
    /// The job's shared Docker-action volumes, if the pre-pass provisioned
    /// them (`super::docker_action` module docs). Present whenever the job
    /// contains a Docker action at *any* nesting depth, which is exactly
    /// when a nested one can be reached.
    pub docker_volumes: Option<docker_action::SiblingVolumes>,
    /// The job's runner/base defaults (`crate::executor::step::layered_env`'s
    /// `base` layer — includes the container's own live `PATH`, seeded by
    /// `crate::executor::job::seed_container_path`). A nested step is
    /// otherwise part of the *same* job-wide environment an ordinary
    /// top-level step sees (module docs: "env: accumulation is job-wide, not
    /// composite-scoped") — that applies to this static layer too, not only
    /// to `GITHUB_ENV` accumulation.
    pub base_env: &'a IndexMap<String, String>,
    /// The job's resolved workflow-level `env:`.
    pub workflow_env: &'a IndexMap<String, String>,
    /// The job's resolved job-level `env:`.
    pub job_env: &'a IndexMap<String, String>,
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
/// This is the entry point `crate::executor::step` calls for a top-level
/// composite step, so its signature is fixed; it has no live caller
/// [`Context`] to hand in (that module builds one only to resolve `with:`,
/// never passes it here), so it defers to [`execute_inner`] with `None` —
/// see [`fallback_caller_context`] for what that costs. A *nested* composite
/// (`run_nested_uses`'s `ResolvedUses::Composite` arm, this same module)
/// calls [`execute_inner`] directly with its own already-built `ctx`
/// instead of going through this wrapper, so it does not pay that cost.
///
/// # Errors
/// Returns [`ExecError`] on any nested engine or evaluation failure.
pub(crate) async fn execute(
    env: &CompositeEnv<'_>,
    state: &mut CompositeState<'_>,
    resolved: &ResolvedComposite,
    with: &IndexMap<String, String>,
    outer_step_index: usize,
) -> Result<CompositeOutcome, ExecError> {
    execute_inner(env, state, resolved, with, outer_step_index, None).await
}

/// [`execute`]'s real body, additionally taking the enclosing scope's own
/// already-built [`Context`] when the caller has one on hand (a nested
/// composite's recursive call), so a declared input's `default:` evaluates
/// against the *same* scope that resolved `with:` — matching the pinned
/// runner's `ActionManifestManager.EvaluateDefaultInput`, which runs against
/// the invoking step's own `IExecutionContext`, the identical scope
/// `EvaluateStepInputs` resolves `with:` against.
async fn execute_inner(
    env: &CompositeEnv<'_>,
    state: &mut CompositeState<'_>,
    resolved: &ResolvedComposite,
    with: &IndexMap<String, String>,
    outer_step_index: usize,
    caller_ctx: Option<&Context>,
) -> Result<CompositeOutcome, ExecError> {
    let default_ctx = match caller_ctx {
        Some(ctx) => ctx.clone(),
        None => fallback_caller_context(env, state),
    };
    let inputs_value = composite_inputs_value(&resolved.inputs, with, &default_ctx)?;

    let mut composite_records: Vec<StepRecord> = Vec::new();
    // The composite's *own* rolling status, mirroring the top-level job
    // step loop's `state.status` (`crate::executor::step`) but scoped to
    // this composite instance — starts `Success` regardless of the
    // enclosing job's status, matching the pinned runner's `action_status`
    // (see `build_composite_context`'s doc comment for the verified rule
    // and source citation).
    let mut composite_status = RunStatus::Success;

    for (nested_index, step) in resolved.steps.iter().enumerate() {
        let state_key = outer_step_index * 1000 + nested_index;
        let ctx = build_composite_context(
            env,
            state,
            &inputs_value,
            &composite_records,
            composite_status,
        );
        let label = state
            .masker
            .apply(&nested_step_label(step, nested_index, &ctx)?);

        let condition_true = match &step.source.if_condition {
            Some(raw) => {
                template::resolve_condition(raw, &ctx).map_err(ExecError::template_eval)?
            }
            None => true,
        };
        // Whether this step's own `if:` (when declared) already references a
        // status-check function — if not, GitHub wraps it in an implicit
        // `success() &&`, applied below via `step_activates` exactly as the
        // top-level step loop applies it (`crate::executor::step`'s own
        // `implicit_status_gate`/`step_activates` call).
        let implicit_status_gate = match &step.source.if_condition {
            Some(raw) => !condition_calls_status_function(raw),
            None => true,
        };
        let activates = step_activates(composite_status, implicit_status_gate, condition_true);
        if !activates {
            if let Some(id) = &step.source.id {
                composite_records.push(StepRecord {
                    id: id.clone(),
                    outcome: Conclusion::Skipped,
                    conclusion: Conclusion::Skipped,
                    outputs: IndexMap::new(),
                });
            }
            let _ = writeln!(state.out, "    \u{2013} {label} (skipped)");
            continue;
        }

        let _ = writeln!(state.out, "    \u{25b6} {label}");
        let outcome = run_nested_step(
            env,
            state,
            step,
            &ctx,
            &resolved.action_path,
            state_key,
            &label,
        )
        .await?;
        let continue_on_error = resolve_continue_on_error(&step.source, &ctx)?;
        let result = step_result_from_exit(outcome.exit, continue_on_error);
        let _ = writeln!(
            state.out,
            "      {} {label}",
            status_sigil(result.conclusion)
        );
        if let Some(id) = &step.source.id {
            composite_records.push(StepRecord {
                id: id.clone(),
                outcome: result.outcome,
                conclusion: result.conclusion,
                outputs: outcome.outputs,
            });
        }
        composite_status = advance_status(composite_status, result.conclusion);
    }

    // Unlike the per-nested-step context above, the composite's own
    // `outputs:` mapping is evaluated by the pinned runner's
    // `CompositeActionHandler.ProcessOutputs` against the composite's *own*
    // top-level `ExecutionContext` — which is not itself `IsEmbedded`, so a
    // status function there falls to `SuccessFunction`/`FailureFunction`'s
    // non-composite branch (`executionContext.JobContext.Status`) rather
    // than the composite's own `action_status`. `env.status` — the
    // enclosing job's rolling status, unaffected by this composite's own
    // nested steps — is the right value here, not `composite_status`.
    let final_ctx =
        build_composite_context(env, state, &inputs_value, &composite_records, env.status);
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
/// default — evaluated as a `${{ }}` template against `ctx`, the enclosing
/// scope that resolved this composite's own `with:` (see [`execute_inner`]) —
/// overridden by the (already-resolved) `with:` map. The default-evaluation
/// step mirrors `super::nodejs::input_env` exactly (the pinned runner's
/// `ActionManifestManager.EvaluateDefaultInput` evaluates a declared input's
/// default generically for every action kind, not only composite's); this
/// function used to insert `input.default` as a raw literal, so `default:
/// ${{ github.repository }}` resolved to the four literal characters `${{`
/// instead of the repository name.
fn composite_inputs_value(
    declared: &IndexMap<String, ActionInput>,
    with: &IndexMap<String, String>,
    ctx: &Context,
) -> Result<Value, ExecError> {
    let mut effective: IndexMap<String, String> = IndexMap::new();
    for (id, input) in declared {
        if let Some(default) = &input.default {
            let value =
                template::resolve_template(default, ctx).map_err(ExecError::template_eval)?;
            effective.insert(id.clone(), value);
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

/// The best-available enclosing [`Context`] for evaluating a *top-level*
/// composite's own declared input `default:` values, used only when
/// [`execute`] (called from `crate::executor::step`, which never hands this
/// module a live `Context`) has no caller context to thread in — a nested
/// composite's own defaults always go through [`execute_inner`]'s
/// `caller_ctx: Some(..)` path instead (`run_nested_uses`'s
/// `ResolvedUses::Composite` arm), with full fidelity.
///
/// `github`/`vars`/`inputs` (the *workflow*-level `inputs`, e.g.
/// `workflow_dispatch` inputs — this composite's own declared inputs are a
/// distinct, inner scope `build_composite_context` introduces later)/
/// `secrets`/`runner`/`matrix`/`env`/`needs`/`status` all come through as
/// the enclosing job step would see them (`CompositeEnv` carries the same
/// roots/env layers/status `crate::executor::step` built its own context
/// from). `steps` is the one root this fallback cannot reproduce: neither
/// `CompositeEnv` nor `CompositeState` carries the job's `StepRecord`
/// history, so it is left empty here. This is a narrow, deliberate fidelity
/// gap — a top-level composite's input default referencing `steps.*` would
/// not resolve the way `EvaluateDefaultInput` (run against the same
/// `IExecutionContext` `with:` was resolved against, `steps` included)
/// does — rather than an invented value.
fn fallback_caller_context(env: &CompositeEnv<'_>, state: &CompositeState<'_>) -> Context {
    let full_env = layer_step_env(EnvLayers {
        base: env.base_env,
        workflow: env.workflow_env,
        job: env.job_env,
        accumulated: state.job_accumulated,
        step: &IndexMap::new(),
    });
    build_context(
        env.roots,
        &LiveState {
            runner: env.runner_ctx,
            matrix: env.matrix,
            env: &full_env,
            steps: &[],
            needs: env.needs,
            status: env.status,
        },
    )
}

/// Whether `raw`'s parsed expression references any of the four status
/// check functions (`success`/`failure`/`cancelled`/`always`) — this
/// composite step's own equivalent of `greenlit-engine`'s workflow-level
/// implicit-gate detection (`crate::plan::conditions::expr_calls_status`,
/// private to that crate), needed here because a composite step's raw `if:`
/// text never goes through that crate's planner at all (module docs: this
/// crate evaluates manifest-sourced text live, with no static/deferred
/// split). A parse failure is treated as "no status function" —
/// conservative, and harmless: `template::resolve_condition` on the same raw
/// text surfaces the real parse error at the call site right above.
fn condition_calls_status_function(raw: &str) -> bool {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
        .map(str::trim)
        .unwrap_or(trimmed);
    match greenlit_expr::parse(inner) {
        Ok(expr) => expr_calls_status_function(&expr),
        Err(_) => false,
    }
}

/// A nested step's display label: its `name:` (resolved), else `id:`, else
/// its `uses:` reference (more informative than a bare position for a
/// nested action step), else a positional label — the same fallback chain
/// `crate::executor::step::step_label` applies to a top-level step, plus the
/// `uses:` tier (a top-level `uses:` step's header line uses only
/// name/id/position too, but a composite's own steps are typically
/// unnamed `uses:` entries, where "step 2" would otherwise be the only
/// available label).
fn nested_step_label(
    step: &ResolvedCompositeStep,
    nested_index: usize,
    ctx: &Context,
) -> Result<String, ExecError> {
    if let Some(name) = &step.source.name {
        return template::resolve_template(name, ctx).map_err(ExecError::template_eval);
    }
    if let Some(id) = &step.source.id {
        return Ok(id.clone());
    }
    if let Some(uses) = &step.source.uses {
        return Ok(uses.clone());
    }
    Ok(format!("step {}", nested_index + 1))
}

/// A short sigil for a nested step's conclusion in the live status line —
/// the same glyphs `crate::executor::step::status_sigil` prints for a
/// top-level step (private to that module, so duplicated here rather than
/// exported; diagnostics text is not snapshot-pinned, per this task's own
/// instructions).
fn status_sigil(conclusion: Conclusion) -> &'static str {
    match conclusion {
        Conclusion::Success => "\u{2713}",
        Conclusion::Failure => "\u{2717}",
        Conclusion::Cancelled => "\u{29b8}",
        Conclusion::Skipped => "\u{2013}",
    }
}

/// Runs one nested step: a plain `run:` shell command, or another resolved
/// action (recursing for a nested composite). `action_path` is the
/// enclosing composite's own root directory, exposed to a nested `run:` as
/// `GITHUB_ACTION_PATH` exactly as GitHub does for a composite's own steps.
/// `label` is this step's already-resolved, already-masked display label
/// (`nested_step_label`), used only by the `run:` path below for its
/// step-summary-overflow warning — the `uses:` dispatch
/// ([`run_nested_uses`]) prints its own, `reference`-keyed warning instead.
async fn run_nested_step(
    env: &CompositeEnv<'_>,
    state: &mut CompositeState<'_>,
    step: &ResolvedCompositeStep,
    ctx: &Context,
    action_path: &str,
    state_key: usize,
    label: &str,
) -> Result<NestedStepResult, ExecError> {
    if step.uses.is_some() {
        return run_nested_uses(env, state, step, ctx, state_key).await;
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
    // `paths.script` (not a hand-reconstructed `{cmdfiles_dir}/script`) is
    // where `cmdfiles::prepare` below actually materializes the script:
    // `CommandFilePaths::new` nests everything one `step-<index>` directory
    // deeper than `cmdfiles_dir` itself. Resolving the shell against the
    // wrong path here previously pointed the exec at a file that was never
    // written, failing every composite `run:` step with "no such file".
    let paths = CommandFilePaths::new(&cmdfiles_dir, 0);
    let shell = greenlit_engine::execution::shell::resolve_shell(
        greenlit_engine::execution::shell::ShellSelection {
            step_shell: Some(&shell_resolved),
            default_shell: None,
            in_container: false,
            bash_available: true,
        },
        &paths.script,
    )
    .map_err(|source| ExecError::Shell {
        label: "composite step".to_string(),
        source,
    })?;

    let full_env = nested_full_env(env, state, &step.source, ctx)?;

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
    let output = env.engine.exec(env.container, &spec, &mut sink).await;
    sink.finish()?;
    let output = output?;
    let exit = if output.exit_code == 0 {
        StepExit::Success
    } else {
        StepExit::Failed
    };

    let effects = cmdfiles::collect(env.engine, env.container, &paths)
        .await
        .map_err(|source| ExecError::CommandFile(state.masker.apply(&source.to_string())))?;
    let outputs =
        cmdfiles::apply_effects(&effects, state.job_accumulated, state.job_path_additions);
    if !effects.summary_within_limit {
        // GitHub drops an over-cap job summary and notes it; mirror that —
        // the same line every other nested/top-level call site already
        // prints. `run_nested_step`'s `run:` path was the one place that
        // silently dropped this signal, having no printable label of its
        // own before this wave.
        let _ = writeln!(
            state.out,
            "  [warning] {label}: GITHUB_STEP_SUMMARY exceeded the 1 MiB limit and was dropped"
        );
    }
    Ok(NestedStepResult { exit, outputs })
}

/// The fully-layered environment for one nested step: the job's own
/// base/workflow/job layers plus its `GITHUB_ENV` accumulation (module docs:
/// env accumulation is job-wide, not composite-scoped) with this step's own
/// `env:` on top, and `GITHUB_PATH` additions applied.
fn nested_full_env(
    env: &CompositeEnv<'_>,
    state: &CompositeState<'_>,
    step: &CompositeStep,
    ctx: &Context,
) -> Result<IndexMap<String, String>, ExecError> {
    let mut step_env = IndexMap::new();
    for (name, value) in &step.env {
        step_env.insert(
            name.clone(),
            template::resolve_template(value, ctx).map_err(ExecError::template_eval)?,
        );
    }
    let mut full_env = layer_step_env(EnvLayers {
        base: env.base_env,
        workflow: env.workflow_env,
        job: env.job_env,
        accumulated: state.job_accumulated,
        step: &step_env,
    });
    apply_path_additions(&mut full_env, state.job_path_additions);
    Ok(full_env)
}

/// Evaluates a nested step's still-raw `with:` text against the enclosing
/// composite's context.
fn resolve_with(
    with_raw: &IndexMap<String, String>,
    ctx: &Context,
) -> Result<IndexMap<String, String>, ExecError> {
    with_raw
        .iter()
        .map(|(key, value)| template::resolve_template(value, ctx).map(|v| (key.clone(), v)))
        .collect::<Result<_, template::TemplateError>>()
        .map_err(ExecError::template_eval)
}

/// Runs a nested `uses:` step's main phase (dispatching by kind), running
/// its `pre:` immediately beforehand and registering its `post:` onto the
/// shared job-wide chain — see module docs' "Scope decision". `ctx` is the
/// *enclosing* composite's context (its `inputs`/`steps`/blocked `secrets`),
/// against which this nested step's own still-raw `with:` text is evaluated.
///
/// # Errors
/// Returns [`ExecError`] on any nested engine or evaluation failure, or if
/// the step is somehow reached without a resolved `uses:` (defensive; the
/// caller already checked).
async fn run_nested_uses(
    env: &CompositeEnv<'_>,
    state: &mut CompositeState<'_>,
    step: &ResolvedCompositeStep,
    ctx: &Context,
    state_key: usize,
) -> Result<NestedStepResult, ExecError> {
    let Some(uses) = &step.uses else {
        return Err(ExecError::Infrastructure {
            message: "composite step dispatched as `uses:` without a resolved action".to_string(),
            fix: "internal error: report this as a Greenlit defect".to_string(),
        });
    };
    let reference = step.source.uses.as_deref().unwrap_or("nested action");
    let with_raw = &step.source.with;
    match &**uses {
        ResolvedUses::Checkout => {
            let with = resolve_with(with_raw, ctx)?;
            let outcome = checkout::execute(
                checkout::CheckoutRequest {
                    engine: env.engine,
                    container: env.container,
                    step_span: env.step_span,
                    with: &with,
                    runner_env: env.runner_env,
                    workspace: env.workspace,
                    github_token: env.github_token,
                },
                state.masker,
            )
            .await?;
            // Onto the same job-wide LIFO chain a top-level checkout uses,
            // so its credential cleanup runs at job end in reverse order
            // with every other post step (`super::post_chain`).
            state.post_chain.push(PostEntry {
                label: format!("Post {reference}"),
                action: PostAction::Checkout(outcome.post),
            });
            Ok(NestedStepResult {
                exit: StepExit::Success,
                outputs: outcome.outputs,
            })
        }
        ResolvedUses::Docker(docker) => {
            let Some(volumes) = env.docker_volumes else {
                return Err(ExecError::Infrastructure {
                    message: format!(
                        "'{reference}' is a Docker action, but this job's shared volumes were not provisioned"
                    ),
                    fix: "internal error: report this as a Greenlit defect".to_string(),
                });
            };
            let with = resolve_with(with_raw, ctx)?;
            let full_env = nested_full_env(env, state, &step.source, ctx)?;
            let paths =
                CommandFilePaths::new(&nested_cmdfiles_dir(env.cmdfiles_base, state_key), 0);
            let outcome = docker_action::run_step(
                docker_action::DockerActionRequest {
                    engine: env.engine,
                    container: env.container,
                    resolved: docker,
                    reference,
                    with: &with,
                    full_env: &full_env,
                    ctx,
                    workspace: env.workspace,
                    cmdfiles_root: env.cmdfiles_base,
                    cmdfiles: &paths,
                    volumes,
                    volume_namespace: env.volume_namespace,
                    locked_images: env.locked_images,
                },
                state.out,
                state.masker,
            )
            .await?;
            let outputs = cmdfiles::apply_effects(
                &outcome.effects,
                state.job_accumulated,
                state.job_path_additions,
            );
            if !outcome.effects.summary_within_limit {
                // GitHub drops an over-cap job summary and notes it; mirror that.
                let _ = writeln!(
                    state.out,
                    "  [warning] {reference}: GITHUB_STEP_SUMMARY exceeded the 1 MiB limit and was dropped"
                );
            }
            Ok(NestedStepResult {
                exit: outcome.exit,
                outputs,
            })
        }
        ResolvedUses::Composite(nested) => {
            let with = resolve_with(with_raw, ctx)?;
            // `Some(ctx)`, not `execute`'s own `None` default: `ctx` here is
            // this (enclosing) composite's own context, the exact scope
            // that just resolved `with` above — the nested composite's own
            // input defaults must evaluate against it, not the fallback
            // (`fallback_caller_context`'s doc comment).
            let outcome = Box::pin(execute_inner(
                env,
                state,
                nested,
                &with,
                state_key,
                Some(ctx),
            ))
            .await?;
            Ok(NestedStepResult {
                exit: outcome.exit,
                outputs: outcome.outputs,
            })
        }
        ResolvedUses::Node(node) => {
            let with = resolve_with(with_raw, ctx)?;
            let input_env = nodejs::input_env(reference, &node.inputs, &with, ctx)?;
            let variant =
                node_runtime::ensure_variant(env.engine, env.container, state.node_variant).await?;
            let node_binary = nodejs::node_binary(env.node_mounts, node.runs.using, variant)?;
            // The job's fully-layered environment (base/workflow/job +
            // `GITHUB_ENV` accumulation + this nested step's own `env:`) —
            // the same shape a top-level JS action's `full_env` has
            // (`crate::executor::step::layered_env`) and this same module's
            // nested `run:` path already builds (`nested_full_env`, used a
            // few lines above for the `Docker` arm). Before this fix, a
            // nested JS action's pre/main phases got only
            // `state.job_accumulated` — the `GITHUB_ENV` accumulation alone,
            // missing `GITHUB_*`/`RUNNER_*`/the seeded `PATH`, workflow
            // `env:`, and job `env:` entirely.
            let full_env = nested_full_env(env, state, &step.source, ctx)?;

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
                            full_env: &full_env,
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
                    full_env: &full_env,
                    extra_env: &input_env,
                    cmdfiles_base: env.cmdfiles_base,
                    phase_key: &format!("{state_key}-main"),
                    working_dir: env.workspace,
                },
                state.out,
                state.masker,
            )
            .await?;
            // Fold the phase's command files into the *job's* accumulation —
            // env and PATH are job-wide, not composite-scoped (module docs).
            // Before `apply_effects` unified this, the nested branch dropped
            // `env`/`path_additions` on the floor while the top-level JS
            // branch merged them: a nested JS action's `GITHUB_ENV` export
            // silently vanished.
            let outputs = cmdfiles::apply_effects(
                &main_outcome.effects,
                state.job_accumulated,
                state.job_path_additions,
            );
            if !main_outcome.effects.summary_within_limit {
                // GitHub drops an over-cap job summary and notes it; mirror that.
                let _ = writeln!(
                    state.out,
                    "  [warning] {reference}: GITHUB_STEP_SUMMARY exceeded the 1 MiB limit and was dropped"
                );
            }
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
                        input_env,
                        step_env: IndexMap::new(),
                        node_version: node.runs.using,
                        state_key,
                    })),
                });
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

/// Builds the context a composite's own nested step (`if:`/`with:`/`run:`)
/// or `outputs:` mapping evaluates against.
///
/// `status` is deliberately a parameter, not read off `env.status`
/// internally: the pinned runner's `SuccessFunction`/`FailureFunction`
/// (`src/Runner.Worker/Expressions/{Success,Failure}Function.cs`,
/// `actions/runner` v2.336.0) evaluate a *nested* step's `success()`/
/// `failure()` against `action_status` — the composite's own rolling
/// result — while every other caller (a composite's `outputs:` mapping, or
/// any non-embedded/non-main-stage step) still falls to
/// `executionContext.JobContext.Status`, the enclosing *job's* rolling
/// status:
/// ```csharp
/// // Decide based on 'action_status' for composite MAIN steps and 'job.status' for pre, post and job-level steps
/// var isCompositeMainStep = executionContext.IsEmbedded && executionContext.Stage == ActionRunStage.Main;
/// if (isCompositeMainStep) {
///     ActionResult actionStatus = EnumUtil.TryParse<ActionResult>(executionContext.GetGitHubContext("action_status")) ?? ActionResult.Success;
///     return actionStatus == ActionResult.Success; // ActionResult.Failure for FailureFunction
/// } else {
///     ActionResult jobStatus = executionContext.JobContext.Status ?? ActionResult.Success;
///     return jobStatus == ActionResult.Success;
/// }
/// ```
/// `action_status` itself is set, right before each nested step's condition
/// is evaluated, from the composite handler's *own* rolling
/// `ExecutionContext.Result` (`src/Runner.Worker/Handlers/
/// CompositeActionHandler.cs`'s `RunStepsAsync`):
/// ```csharp
/// var actionResult = ExecutionContext.Result?.ToActionResult() ?? ActionResult.Success;
/// step.ExecutionContext.SetGitHubContext("action_status", actionResult.ToString().ToLowerInvariant());
/// // ...later, after the embedded step runs...
/// if (step.ExecutionContext.Result == TaskResult.Failed || step.ExecutionContext.Result == TaskResult.Canceled) {
///     ExecutionContext.Result = TaskResultUtil.MergeTaskResults(ExecutionContext.Result, step.ExecutionContext.Result.Value);
/// }
/// ```
/// That rolling `ExecutionContext.Result` starts unset (`?? ActionResult.Success`)
/// independent of the enclosing job's own status — exactly `execute_inner`'s
/// `composite_status`, seeded `RunStatus::Success` every call. Per-nested-step
/// callers therefore pass `composite_status`; the one caller evaluating this
/// composite's own `outputs:` (not itself `IsEmbedded`, so it falls to the
/// `else` branch above) passes `env.status`, the enclosing job's status,
/// instead — see [`execute_inner`]'s `final_ctx` doc comment.
///
/// (`cancelled()` does not special-case the composite-main-step branch at
/// all — `CancelledFunction.cs` always reads `executionContext.JobContext.Status`,
/// even for an embedded main step. `greenlit_expr::Context` has one
/// `RunStatus` field driving all four status functions together, not a
/// second per-function axis, so this implementation cannot reproduce that
/// one narrower divergence without changing `greenlit-expr` (out of this
/// task's allowed files); recorded as a known, narrow gap rather than
/// silently assumed away.)
fn build_composite_context(
    env: &CompositeEnv<'_>,
    state: &CompositeState<'_>,
    inputs_value: &Value,
    composite_records: &[StepRecord],
    status: RunStatus,
) -> Context {
    // The `env` context is job-wide (module docs: "env: accumulation is
    // job-wide, not composite-scoped"), so a nested step's `${{ env.* }}`
    // sees the same base/workflow/job layers plus `GITHUB_ENV` accumulation
    // an ordinary top-level step's context would — not only the
    // accumulation, matching `crate::executor::step::layered_env`.
    let full_env = layer_step_env(EnvLayers {
        base: env.base_env,
        workflow: env.workflow_env,
        job: env.job_env,
        accumulated: state.job_accumulated,
        step: &IndexMap::new(),
    });
    Context::new(Arc::clone(&env.roots.fs))
        .with_github(env.roots.github.clone())
        .with_vars(env.roots.vars.clone())
        .with_inputs(inputs_value.clone())
        // Blocked per module docs' ADR citation: a composite never sees the
        // run's real secrets, only whatever it receives through `inputs`.
        .with_secrets(Value::object(vec![]))
        .with_runner(env.runner_ctx.clone())
        .with_matrix(env.matrix.clone())
        .with_env(env_value(&full_env))
        .with_steps(build_steps_context(composite_records))
        .with_needs(greenlit_engine::execution::build_needs_context(env.needs))
        .with_status(status)
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
