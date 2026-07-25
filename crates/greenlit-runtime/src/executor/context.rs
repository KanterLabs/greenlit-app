//! Assembling live expression [`Context`]s at execution time, and the small
//! captures the executor needs to finish plan-time-deferred values.
//!
//! Phase 1 folded every context-sensitive field as far as the static contexts
//! allowed, leaving a residual for anything needing runtime data. This module
//! rebuilds the full context (live `env`/`steps`/`needs`/`runner`/`matrix`) so
//! [`greenlit_engine::execution::resolve`] can finish those residuals against
//! real runtime state — never inventing a value earlier.

use std::sync::Arc;

use indexmap::IndexMap;

use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::execution::resolve::resolve_string;
use greenlit_engine::execution::{
    NeedRecord, StepRecord, build_needs_context, build_steps_context,
};
use greenlit_engine::pass_through::EnvValue;
use greenlit_expr::{Context, EvalError, HashFilesFs, RunStatus, Value};

use crate::engine::ExecOutputSink;

/// The run-wide context roots that do not change between steps.
pub(crate) struct ContextRoots {
    /// The `github` context (from the synthetic event).
    pub github: Value,
    /// The `vars` context (already resolved by the CLI).
    pub vars: Value,
    /// The `inputs` context (`workflow_dispatch` inputs, else empty).
    pub inputs: Value,
    /// The `secrets` context (empty in Phase 2; auth lands in Phase 3).
    pub secrets: Value,
    /// Backing filesystem for `hashFiles()`, rooted at the repo.
    pub fs: Arc<dyn HashFilesFs>,
}

/// A job instance's live state at one point during execution — the inputs to a
/// step-time [`Context`] beyond the run-wide [`ContextRoots`].
pub(crate) struct LiveState<'a> {
    /// The `runner` context object.
    pub runner: &'a Value,
    /// The `matrix` context (`Null` for a non-matrix job).
    pub matrix: &'a Value,
    /// The current `env` layer (base + workflow + job + GITHUB_ENV accumulation).
    pub env: &'a IndexMap<String, String>,
    /// The `steps` context contributions so far.
    pub steps: &'a [StepRecord],
    /// The `needs` context from completed direct dependencies.
    pub needs: &'a [NeedRecord],
    /// The rolling job status the status-check functions evaluate against.
    pub status: RunStatus,
}

/// Build a step-time [`Context`] from the run roots plus the job's live state.
pub(crate) fn build_context(roots: &ContextRoots, live: &LiveState<'_>) -> Context {
    Context::new(Arc::clone(&roots.fs))
        .with_github(roots.github.clone())
        .with_vars(roots.vars.clone())
        .with_inputs(roots.inputs.clone())
        .with_secrets(roots.secrets.clone())
        .with_runner(live.runner.clone())
        .with_matrix(live.matrix.clone())
        .with_env(env_to_value(live.env))
        .with_steps(build_steps_context(live.steps))
        .with_needs(build_needs_context(live.needs))
        .with_status(live.status)
}

/// The `env` context root as an object of string values.
fn env_to_value(env: &IndexMap<String, String>) -> Value {
    Value::object(
        env.iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}

/// The `runner` context object (`runner.os`, `runner.arch`, …) derived from the
/// runner environment. v0 targets Linux x86_64, so `os`/`arch` are fixed.
pub(crate) fn runner_context(runner_env: &RunnerEnv) -> Value {
    Value::object(vec![
        ("os".to_string(), Value::String("Linux".to_string())),
        ("arch".to_string(), Value::String("X64".to_string())),
        (
            "name".to_string(),
            Value::String(runner_env.runner_name.clone()),
        ),
        (
            "temp".to_string(),
            Value::String(runner_env.runner_temp.clone()),
        ),
        (
            "tool_cache".to_string(),
            Value::String(runner_env.runner_tool_cache.clone()),
        ),
        (
            "workspace".to_string(),
            Value::String(runner_env.workspace.clone()),
        ),
    ])
}

/// Overlays this job instance's runtime-only `github` context properties
/// onto the run's synthetic `github` object — exactly the subset of
/// `crate::partial_eval::classify::DEFERRED_GITHUB_PROPERTIES` (see
/// `greenlit_engine`) this crate can now truthfully supply once a job's own
/// container and command-file paths exist: `event_path`
/// (`crate::executor::cmdfiles::event_file_path`), `workspace`, `job`,
/// `run_id`, `run_number`, `run_attempt`. The rest of that list —
/// `action`/`action_path`/`token`/... — stays absent; Phase 1's own "never
/// invent a value" reasoning (`greenlit_engine::event` module docs) still
/// applies to them.
///
/// `ObjectValue::new`'s last-value-wins, ordinal-ignore-case dedup
/// (`greenlit_expr::value`) means each of these safely overrides any
/// same-named entry the synthetic event happened to already carry, so this
/// is a plain append rather than a search-and-replace. `runner_env` must
/// already have this job's own `job`/`workspace` set (`RunnerEnv::job`,
/// `RunnerEnv::workspace` — see `crate::executor::job::run_instance`), since
/// those two are per-instance, unlike `run_id`/`run_number`/`run_attempt`
/// which are the same for every job in the run.
pub(crate) fn job_github_context(base: &Value, runner_env: &RunnerEnv, event_path: &str) -> Value {
    let Value::Object(object) = base else {
        return base.clone();
    };
    let mut entries: Vec<(String, Value)> = object
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect();
    entries.extend([
        ("job".to_string(), Value::String(runner_env.job.clone())),
        (
            "event_path".to_string(),
            Value::String(event_path.to_string()),
        ),
        (
            "workspace".to_string(),
            Value::String(runner_env.workspace.clone()),
        ),
        (
            "run_id".to_string(),
            Value::String(runner_env.run_id.clone()),
        ),
        (
            "run_number".to_string(),
            Value::String(runner_env.run_number.clone()),
        ),
        (
            "run_attempt".to_string(),
            Value::String(runner_env.run_attempt.clone()),
        ),
    ]);
    Value::object(entries)
}

/// Copies `github` context properties GitHub also exposes as `GITHUB_*` env
/// vars straight off the already-assembled `github` object, rather than
/// re-deriving them a second time from raw git/event components:
///
/// - `GITHUB_WORKFLOW_REF`/`GITHUB_WORKFLOW_SHA`: `greenlit_engine::event`
///   always sets `workflow_ref`; `workflow_sha` only when the local
///   workflow file provably matches the commit it claims to come from
///   (absent, not invented, otherwise — see that module's doc comment).
///   Reading them back off `github` rather than recomputing keeps this
///   single-sourced.
/// - `GITHUB_HEAD_REF`/`GITHUB_BASE_REF`: present only for a synthetic
///   pull-request event. Citation: the pinned runner's own
///   `GitHubContext.cs:52-68` emits these env vars only when the key exists
///   in the service-sent `github` context (an allowlisted iteration, not an
///   always-present-but-possibly-empty pair); docs: "only set when the
///   event that triggers a workflow run is either pull_request or
///   pull_request_target". `litci` only ever populates `head_ref`/
///   `base_ref` on `github.event`'s object for a synthetic pull_request
///   event (`greenlit_engine::event::payload::pull_request_payload`), so
///   mirroring "present in `github` ⇒ present as an env var" reproduces the
///   same absent-not-empty rule without re-deriving the event kind here.
pub(crate) fn copy_event_env_vars(base_env: &mut IndexMap<String, String>, github: &Value) {
    let Value::Object(object) = github else {
        return;
    };
    for key in ["head_ref", "base_ref", "workflow_ref", "workflow_sha"] {
        if let Some(Value::String(value)) = object.get(key) {
            base_env.insert(format!("GITHUB_{}", key.to_uppercase()), value.clone());
        }
    }
}

/// Resolve one planned `env:`/`with:` layer against `ctx`, finishing any
/// deferred interpolation.
///
/// # Errors
///
/// Returns the first [`EvalError`] a residual value produced.
pub(crate) fn resolve_env_layer(
    entries: &IndexMap<String, EnvValue>,
    ctx: &Context,
) -> Result<IndexMap<String, String>, EvalError> {
    let mut resolved = IndexMap::with_capacity(entries.len());
    for (name, planned) in entries {
        resolved.insert(name.clone(), resolve_string(planned, ctx)?);
    }
    Ok(resolved)
}

/// An [`ExecOutputSink`] that captures stdout into a byte buffer — used to read
/// a step's command files back out of the container. Stderr is discarded.
#[derive(Default)]
pub(crate) struct CaptureSink {
    /// The collected standard output.
    pub stdout: Vec<u8>,
}

impl CaptureSink {
    /// The captured bytes as a lossy UTF-8 string.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

impl ExecOutputSink for CaptureSink {
    fn on_stdout(&mut self, chunk: &[u8]) {
        self.stdout.extend_from_slice(chunk);
    }
    fn on_stderr(&mut self, _chunk: &[u8]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner_env() -> RunnerEnv {
        RunnerEnv {
            job: "build".to_string(),
            workspace: "/home/runner/work/r/r".to_string(),
            run_id: "1".to_string(),
            run_number: "1".to_string(),
            run_attempt: "1".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn job_github_context_adds_the_runtime_only_properties_without_disturbing_others() {
        let base = Value::object(vec![(
            "event_name".to_string(),
            Value::String("push".to_string()),
        )]);
        let patched = job_github_context(&base, &runner_env(), "/greenlit/cmdfiles/event.json");
        let Value::Object(object) = patched else {
            panic!("expected object");
        };
        assert_eq!(
            object.get("event_name"),
            Some(&Value::String("push".to_string()))
        );
        assert_eq!(object.get("job"), Some(&Value::String("build".to_string())));
        assert_eq!(
            object.get("event_path"),
            Some(&Value::String("/greenlit/cmdfiles/event.json".to_string()))
        );
        assert_eq!(
            object.get("workspace"),
            Some(&Value::String("/home/runner/work/r/r".to_string()))
        );
        assert_eq!(object.get("run_id"), Some(&Value::String("1".to_string())));
        assert_eq!(
            object.get("run_number"),
            Some(&Value::String("1".to_string()))
        );
        assert_eq!(
            object.get("run_attempt"),
            Some(&Value::String("1".to_string()))
        );
    }

    #[test]
    fn job_github_context_overrides_a_stale_same_named_entry() {
        // Defensive: even if the synthetic event somehow already carried a
        // `job` entry, the job-instance value must win (last-value-wins
        // dedup, module doc comment).
        let base = Value::object(vec![(
            "job".to_string(),
            Value::String("stale".to_string()),
        )]);
        let patched = job_github_context(&base, &runner_env(), "/x/event.json");
        let Value::Object(object) = patched else {
            panic!("expected object");
        };
        assert_eq!(object.get("job"), Some(&Value::String("build".to_string())));
    }

    #[test]
    fn copy_event_env_vars_present_rule() {
        let github = Value::object(vec![
            (
                "workflow_ref".to_string(),
                Value::String("o/r/.github/workflows/ci.yml@refs/heads/main".to_string()),
            ),
            ("workflow_sha".to_string(), Value::String("a".repeat(40))),
            ("head_ref".to_string(), Value::String("feature".to_string())),
            ("base_ref".to_string(), Value::String("main".to_string())),
        ]);
        let mut base_env = IndexMap::new();
        copy_event_env_vars(&mut base_env, &github);
        assert_eq!(
            base_env.get("GITHUB_WORKFLOW_REF"),
            Some(&"o/r/.github/workflows/ci.yml@refs/heads/main".to_string())
        );
        assert_eq!(base_env.get("GITHUB_WORKFLOW_SHA"), Some(&"a".repeat(40)));
        assert_eq!(
            base_env.get("GITHUB_HEAD_REF"),
            Some(&"feature".to_string())
        );
        assert_eq!(base_env.get("GITHUB_BASE_REF"), Some(&"main".to_string()));
    }

    #[test]
    fn copy_event_env_vars_absent_rule() {
        // A push event's `github` object carries neither `head_ref`/
        // `base_ref` nor (when the workflow file doesn't provably match
        // HEAD) `workflow_sha` — none of those keys are set, so none of the
        // corresponding env vars appear, matching the "absent, not empty"
        // rule this function exists to reproduce.
        let github = Value::object(vec![(
            "workflow_ref".to_string(),
            Value::String("o/r/.github/workflows/ci.yml@refs/heads/main".to_string()),
        )]);
        let mut base_env = IndexMap::new();
        copy_event_env_vars(&mut base_env, &github);
        assert_eq!(
            base_env.get("GITHUB_WORKFLOW_REF"),
            Some(&"o/r/.github/workflows/ci.yml@refs/heads/main".to_string())
        );
        assert!(!base_env.contains_key("GITHUB_WORKFLOW_SHA"));
        assert!(!base_env.contains_key("GITHUB_HEAD_REF"));
        assert!(!base_env.contains_key("GITHUB_BASE_REF"));
    }
}
