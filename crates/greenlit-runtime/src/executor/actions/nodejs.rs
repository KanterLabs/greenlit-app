//! JavaScript action execution: the `INPUT_*`/`STATE_`/pre-post protocol.
//!
//! # Env protocol
//!
//! - `INPUT_<NAME>`: one per effective input (declared default overridden by
//!   `with:`), name uppercased with spaces replaced by underscores — cited
//!   directly from the real runner's own rule,
//!   `src/Runner.Worker/Handlers/Handler.cs` (`actions/runner` v2.336.0,
//!   pinned release — see `super::node_runtime` module docs):
//!   ```csharp
//!   AddEnvironmentVariable(key: $"INPUT_{pair.Key?.Replace(' ', '_').ToUpperInvariant()}", ...)
//!   ```
//! - `GITHUB_ENV`/`GITHUB_OUTPUT`/`GITHUB_PATH`/`GITHUB_STEP_SUMMARY`: the
//!   same four workflow command files `run:` steps already use
//!   (`crate::executor::cmdfiles`) — GitHub does not special-case actions
//!   for these, so this module reuses that exact protocol unchanged.
//! - `GITHUB_STATE`/`STATE_<name>`: `save-state`-style entries written to
//!   `GITHUB_STATE` (same `name=value`/heredoc file format as `GITHUB_ENV`,
//!   confirmed identical by `SaveStateFileCommand`,
//!   `src/Runner.Worker/FileCommandManager.cs`) accumulate in a per-step-
//!   instance state map carried across pre/main/post
//!   (`ExecutionContext.IntraActionState`) and are exposed to each
//!   subsequent phase as `STATE_<name>` (`src/Runner.Worker/ActionRunner.cs`:
//!   `environment[$"STATE_{state.Key}"] = state.Value`) — *not* to the phase
//!   that set them.
//! - **pre/post ordering and defaults**: pre steps for every action in the
//!   job run, in job order, before the job's first main step; post steps
//!   run in the reverse of that order at job end regardless of failure;
//!   both default to `always()` when `pre-if`/`post-if` is not declared.
//!   Verified against `actions/runner` v2.336.0 (pinned release):
//!   `src/Runner.Worker/JobExtension.cs` (`preJobSteps` assembled in job-
//!   step order ahead of the main `jobSteps` list),
//!   `src/Runner.Worker/ExecutionContext.cs`/`StepsRunner.cs`
//!   (`PostJobSteps` is a `Stack<IStep>`, drained with `TryPop` — LIFO —
//!   only once every main step has run), and
//!   `src/Runner.Worker/ActionManifestManager.cs`
//!   (`InitCondition = preIfToken?.Value ?? "always()"`,
//!   `CleanupCondition = postIfToken?.Value ?? "always()"`).
//!
//! Nested Node actions inside a composite deviate from the strict "hoisted
//! to the job's very first step" pre timing for simplicity — see
//! `super::composite` module docs' "Scope decision" for exactly what is and
//! is not reproduced.

use std::collections::HashMap;
use std::io::Write;

use greenlit_actions::manifest::{ActionInput, NodeVersion};
use greenlit_engine::execution::Masker;
use greenlit_engine::execution::command_files::Assignment;
use greenlit_engine::execution::outcome::StepExit;
use greenlit_expr::Context;
use indexmap::IndexMap;

use crate::engine::{ContainerEngine, ExecSpec};
use crate::error::RuntimeError;
use crate::executor::ExecError;
use crate::executor::cmdfiles::{self, CommandFilePaths};
use crate::executor::logsink::StepLogSink;

use super::node_runtime::NodeVariant;
use super::template;

/// Where the pinned Node runtimes are bind-mounted inside the job
/// container, one subdirectory per `(version, variant)` actually needed by
/// the job (`super::resolve`'s pre-pass decides which).
#[derive(Debug, Clone, Default)]
pub(crate) struct NodeRuntimeMounts {
    pub node20_standard: Option<String>,
    pub node20_alpine: Option<String>,
    pub node24_standard: Option<String>,
    pub node24_alpine: Option<String>,
}

impl NodeRuntimeMounts {
    fn path_for(&self, version: NodeVersion, variant: NodeVariant) -> Option<&str> {
        match (version, variant) {
            (NodeVersion::Node20, NodeVariant::Standard) => self.node20_standard.as_deref(),
            (NodeVersion::Node20, NodeVariant::Alpine) => self.node20_alpine.as_deref(),
            (NodeVersion::Node24, NodeVariant::Standard) => self.node24_standard.as_deref(),
            (NodeVersion::Node24, NodeVariant::Alpine) => self.node24_alpine.as_deref(),
        }
    }

    /// Records the in-container path a `(version, variant)` bundle was
    /// mounted at.
    pub(crate) fn set(&mut self, version: NodeVersion, variant: NodeVariant, path: String) {
        let slot = match (version, variant) {
            (NodeVersion::Node20, NodeVariant::Standard) => &mut self.node20_standard,
            (NodeVersion::Node20, NodeVariant::Alpine) => &mut self.node20_alpine,
            (NodeVersion::Node24, NodeVariant::Standard) => &mut self.node24_standard,
            (NodeVersion::Node24, NodeVariant::Alpine) => &mut self.node24_alpine,
        };
        *slot = Some(path);
    }
}

/// Builds the effective `INPUT_<NAME>` environment entries: every declared
/// input's default, overridden by the step's own `with:`, plus every `with:`
/// key not declared at all (GitHub does not require `with:` keys to match
/// declared inputs).
///
/// # Errors
/// Returns [`ExecError::Infrastructure`] if a declared `required: true`
/// input has neither a `with:` value nor a default.
pub(crate) fn input_env(
    action_ref: &str,
    declared: &IndexMap<String, ActionInput>,
    with: &IndexMap<String, String>,
    ctx: &Context,
) -> Result<Vec<(String, String)>, ExecError> {
    let mut effective: HashMap<String, String> = HashMap::new();
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
    for (id, input) in declared {
        if input.required && !effective.contains_key(id) {
            return Err(ExecError::Infrastructure {
                message: format!("'{action_ref}' is missing required input '{id}'"),
                fix: format!("add `with: {id}: <value>` to the step"),
            });
        }
    }
    Ok(effective
        .into_iter()
        .map(|(name, value)| (input_var_name(&name), value))
        .collect())
}

/// `INPUT_<NAME>`'s name transform: replace spaces with underscores, then
/// uppercase — see module docs' citation of the runner's own rule.
fn input_var_name(name: &str) -> String {
    format!("INPUT_{}", name.replace(' ', "_").to_uppercase())
}

/// Picks the in-container Node binary path for `version`, given the probed
/// libc `variant` and this job's mounted runtimes.
///
/// # Errors
/// Returns [`ExecError::Infrastructure`] if the pre-pass did not mount the
/// runtime this action needs (an internal-consistency failure: the pre-pass
/// is supposed to mount every version any resolved action uses).
pub(crate) fn node_binary(
    mounts: &NodeRuntimeMounts,
    version: NodeVersion,
    variant: NodeVariant,
) -> Result<String, ExecError> {
    let dir = mounts
        .path_for(version, variant)
        .ok_or_else(|| ExecError::Infrastructure {
            message:
                "the pinned Node runtime for this action was not mounted into the job container"
                    .to_string(),
            fix: "internal error: report this as a Greenlit defect".to_string(),
        })?;
    Ok(format!("{dir}/bin/node"))
}

/// The result of running one JS action phase (pre, main, or post).
pub(crate) struct PhaseOutcome {
    pub exit: StepExit,
    pub env: Vec<Assignment>,
    pub path_additions: Vec<String>,
    pub outputs: Vec<Assignment>,
    pub state: Vec<Assignment>,
}

/// Everything one JS action phase execution needs, bundled so
/// [`run_phase`] takes one parameter instead of an unwieldy positional list.
pub(crate) struct PhaseRequest<'a> {
    pub engine: &'a dyn ContainerEngine,
    pub container: &'a str,
    pub action_path: &'a str,
    pub script: &'a str,
    pub node_binary: &'a str,
    /// The job's already-fully-layered step environment.
    pub full_env: &'a IndexMap<String, String>,
    /// Layered on top of `full_env`: `INPUT_*`/`STATE_*`.
    pub extra_env: &'a [(String, String)],
    pub cmdfiles_base: &'a str,
    pub phase_key: &'a str,
    pub working_dir: &'a str,
}

/// Runs one script phase (`main`, `pre`, or `post`) of a JS action.
///
/// `request.extra_env` layers `INPUT_*`/`GITHUB_ACTION_PATH`/`STATE_*` on top
/// of the job's already-fully-layered step environment
/// (`request.full_env`); command files (including `GITHUB_STATE`, a fifth
/// file alongside Phase 2's four) follow the identical container-side
/// protocol `crate::executor::cmdfiles` already uses for `run:` steps.
///
/// # Errors
/// Returns an [`ExecError`] on any engine failure or malformed command file.
pub(crate) async fn run_phase(
    request: PhaseRequest<'_>,
    out: &mut (dyn Write + Send),
    masker: &mut Masker,
) -> Result<PhaseOutcome, ExecError> {
    let PhaseRequest {
        engine,
        container,
        action_path,
        script,
        node_binary,
        full_env,
        extra_env,
        cmdfiles_base,
        phase_key,
        working_dir,
    } = request;
    let paths = CommandFilePaths::new(&format!("{cmdfiles_base}/action-{phase_key}"), 0);
    let state_path = format!("{}/state", paths.dir);
    cmdfiles::prepare(engine, container, &paths, "")
        .await
        .map_err(|source| ExecError::CommandFile(masker.apply(&source.to_string())))?;
    // The state file follows the same truncate-then-append protocol as the
    // other four command files, established by the same preparing exec
    // (`cmdfiles::prepare` already created `paths.dir`); truncate it here
    // too, in its own tiny exec, to keep `cmdfiles::prepare`'s signature
    // (four fixed files) unchanged rather than special-casing a fifth file
    // through it.
    truncate_file(engine, container, &state_path).await?;

    let mut env: Vec<(String, String)> = full_env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.extend(extra_env.iter().cloned());
    env.push(("GITHUB_ACTION_PATH".to_string(), action_path.to_string()));
    env.push(("GITHUB_ENV".to_string(), paths.env.clone()));
    env.push(("GITHUB_OUTPUT".to_string(), paths.output.clone()));
    env.push(("GITHUB_PATH".to_string(), paths.path.clone()));
    env.push(("GITHUB_STEP_SUMMARY".to_string(), paths.summary.clone()));
    env.push(("GITHUB_STATE".to_string(), state_path.clone()));

    let script_path = format!("{}/{}", action_path.trim_end_matches('/'), script);
    let spec = ExecSpec {
        cmd: vec![node_binary.to_string(), script_path],
        env,
        working_dir: Some(working_dir.to_string()),
    };
    let mut sink = StepLogSink::new(out, masker);
    let output = engine.exec(container, &spec, &mut sink).await?;
    sink.finish();
    let exit = if output.exit_code == 0 {
        StepExit::Success
    } else {
        StepExit::Failed
    };

    let effects = cmdfiles::collect(engine, container, &paths)
        .await
        .map_err(|source| ExecError::CommandFile(masker.apply(&source.to_string())))?;
    let state_text = read_file(engine, container, &state_path).await?;
    let state = greenlit_engine::execution::command_files::parse_env_file(&state_text)
        .map_err(|source| ExecError::CommandFile(masker.apply(&source.to_string())))?;

    Ok(PhaseOutcome {
        exit,
        env: effects.env,
        path_additions: effects.path_additions,
        outputs: effects.outputs,
        state,
    })
}

async fn truncate_file(
    engine: &dyn ContainerEngine,
    container: &str,
    path: &str,
) -> Result<(), ExecError> {
    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), format!(": > {path}")],
        env: Vec::new(),
        working_dir: None,
    };
    engine
        .exec(container, &spec, &mut crate::engine::SinkNull)
        .await?;
    Ok(())
}

async fn read_file(
    engine: &dyn ContainerEngine,
    container: &str,
    path: &str,
) -> Result<String, RuntimeError> {
    let spec = ExecSpec {
        cmd: vec!["cat".to_string(), path.to_string()],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = crate::executor::context::CaptureSink::default();
    engine.exec(container, &spec, &mut sink).await?;
    Ok(sink.text())
}

#[cfg(test)]
mod tests {
    use super::*;
    use greenlit_expr::{RealFs, Value};
    use std::sync::Arc;

    fn ctx() -> Context {
        Context::new(Arc::new(RealFs::new(std::env::temp_dir())))
            .with_github(Value::object(vec![]))
            .with_env(Value::object(vec![]))
    }

    fn input(default: Option<&str>, required: bool) -> ActionInput {
        ActionInput {
            description: None,
            required,
            default: default.map(str::to_string),
            deprecation_message: None,
        }
    }

    #[test]
    fn input_name_transform_matches_the_runner_rule() {
        assert_eq!(input_var_name("token"), "INPUT_TOKEN");
        assert_eq!(input_var_name("fetch depth"), "INPUT_FETCH_DEPTH");
        assert_eq!(input_var_name("Already-Upper"), "INPUT_ALREADY-UPPER");
    }

    #[test]
    fn with_overrides_a_declared_default() {
        let mut declared = IndexMap::new();
        declared.insert("token".to_string(), input(Some("default-tok"), false));
        let mut with = IndexMap::new();
        with.insert("token".to_string(), "override-tok".to_string());
        let env = input_env("test", &declared, &with, &ctx()).unwrap();
        assert_eq!(
            env,
            vec![("INPUT_TOKEN".to_string(), "override-tok".to_string())]
        );
    }

    #[test]
    fn an_undeclared_with_key_still_becomes_an_input_var() {
        let declared = IndexMap::new();
        let mut with = IndexMap::new();
        with.insert("extra".to_string(), "value".to_string());
        let env = input_env("test", &declared, &with, &ctx()).unwrap();
        assert_eq!(env, vec![("INPUT_EXTRA".to_string(), "value".to_string())]);
    }

    #[test]
    fn a_missing_required_input_fails() {
        let mut declared = IndexMap::new();
        declared.insert("token".to_string(), input(None, true));
        let error = input_env("test/action@v1", &declared, &IndexMap::new(), &ctx()).unwrap_err();
        assert!(error.to_string().contains("required input 'token'"));
    }

    #[test]
    fn a_default_only_input_is_used_when_with_omits_it() {
        let mut declared = IndexMap::new();
        declared.insert("greeting".to_string(), input(Some("hello"), false));
        let env = input_env("test", &declared, &IndexMap::new(), &ctx()).unwrap();
        assert_eq!(
            env,
            vec![("INPUT_GREETING".to_string(), "hello".to_string())]
        );
    }
}
