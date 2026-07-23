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
