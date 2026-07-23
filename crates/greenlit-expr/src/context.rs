//! The typed evaluation context: one [`Value`] per legal `${{ }}` root name.
//!
//! The root-name set, shapes, and availability rules follow GitHub's
//! [Contexts reference](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts).
//! Greenlit implements the eleven roots available to ordinary v0 jobs:
//! `github`, `env`, `vars`, `secrets`, `needs`, `matrix`, `strategy`,
//! `steps`, `runner`, `job`, and `inputs`. The reusable-workflow-only `jobs`
//! context remains outside v0 together with reusable workflows themselves.
//!
//! **Per-key context availability is not modeled here.** Real GitHub
//! additionally restricts which of these eleven roots are legal in a given
//! workflow key (the "Context availability" table — e.g. `secrets` is not
//! legal in `jobs.<id>.if`); this crate treats all eleven roots as always
//! resolvable and leaves narrower per-key allow-lists to the workflow parser
//! and planner, which can inspect the public [`crate::ast::Expr`] tree with
//! the authored workflow-key location in hand.

use std::path::Path;
use std::sync::Arc;

use crate::functions::hash_files::{HashFilesClock, HashFilesFs, SystemHashFilesClock};
use crate::value::Value;

/// The fixed, case-insensitive set of context root names this crate
/// recognizes. The set comes from GitHub's Contexts reference; this stable
/// order keeps parser diagnostics deterministic.
pub(crate) const ROOT_NAMES: [&str; 11] = [
    "github", "env", "vars", "secrets", "needs", "matrix", "strategy", "steps", "runner", "job",
    "inputs",
];

/// The rolling job/step status status-check functions evaluate against.
///
/// GitHub documents these functions under
/// [status check functions](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#status-check-functions).
/// The Actions runner keeps a rolling status (`JobContext.Status`,
/// initially `Success`; becomes `Failure` when a step fails without
/// `continue-on-error`; becomes `Cancelled` on cancellation) in
/// [`StepsRunner.cs`](https://github.com/actions/runner/blob/main/src/Runner.Worker/StepsRunner.cs).
/// Computing
/// *which* status a job/step is currently at (across a `needs` DAG, or
/// across prior steps) is `greenlit-engine`'s planning job; this crate only
/// evaluates the four status functions against whatever status it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunStatus {
    /// No earlier non-`continue-on-error` step/job failed and nothing was
    /// cancelled. The default.
    #[default]
    Success,
    /// A prior non-`continue-on-error` step/job failed.
    Failure,
    /// The run was cancelled.
    Cancelled,
}

/// Everything an expression evaluation needs: one [`Value`] per context
/// root, the current [`RunStatus`] for the status-check functions, and an
/// injected [`HashFilesFs`] and acceleration-only [`HashFilesClock`] for
/// `hashFiles()`.
///
/// Construct with [`Context::new`] (all value roots default to an empty
/// object, `matrix` defaults to `Null` since not every job has a matrix
/// strategy, and status defaults to `Success`), then adjust with the
/// `with_*` builder methods.
#[derive(Debug, Clone)]
pub struct Context {
    github: Value,
    env: Value,
    vars: Value,
    secrets: Value,
    needs: Value,
    matrix: Value,
    strategy: Value,
    steps: Value,
    runner: Value,
    job: Value,
    inputs: Value,
    status: RunStatus,
    fs: Arc<dyn HashFilesFs>,
    hash_files_clock: Arc<dyn HashFilesClock>,
}

impl Context {
    /// Builds a context with every root defaulted to an empty object
    /// (`matrix` to `Null`, since a job without a matrix strategy has no
    /// matrix context data) and [`RunStatus::Success`], backed by `fs` for
    /// `hashFiles()`. The filesystem is shared with a bounded worker because
    /// a timed-out host operation may finish after evaluation has returned.
    pub fn new(fs: Arc<dyn HashFilesFs>) -> Self {
        Context {
            github: Value::object(vec![]),
            env: Value::case_sensitive_object(vec![]),
            vars: Value::object(vec![]),
            secrets: Value::object(vec![]),
            needs: Value::object(vec![]),
            matrix: Value::Null,
            strategy: Value::Null,
            steps: Value::object(vec![]),
            runner: Value::object(vec![]),
            job: Value::object(vec![]),
            inputs: Value::object(vec![]),
            status: RunStatus::default(),
            fs,
            hash_files_clock: Arc::new(SystemHashFilesClock),
        }
    }

    /// Sets the `github` context root (conventionally an `Object`).
    pub fn with_github(mut self, v: Value) -> Self {
        self.github = v;
        self
    }
    /// Sets the `env` context root (conventionally an `Object` flat string
    /// map). Object keys are normalized to case-sensitive lookup because v0
    /// runs on Linux and Actions treats environment variable names as case
    /// sensitive. Nested object values retain their own comparison policy.
    pub fn with_env(mut self, v: Value) -> Self {
        self.env = match v {
            Value::Object(object) => Value::case_sensitive_object(
                object
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.clone()))
                    .collect(),
            ),
            other => other,
        };
        self
    }
    /// Sets the `vars` context root (conventionally an `Object` flat string
    /// map).
    pub fn with_vars(mut self, v: Value) -> Self {
        self.vars = v;
        self
    }
    /// Sets the `secrets` context root (conventionally an `Object` flat
    /// string map).
    pub fn with_secrets(mut self, v: Value) -> Self {
        self.secrets = v;
        self
    }
    /// Sets the `needs` context root (conventionally an `Object` of
    /// `{id: {outputs, result}}`).
    pub fn with_needs(mut self, v: Value) -> Self {
        self.needs = v;
        self
    }
    /// Sets the `matrix` context root (`Null` if the job has no matrix
    /// strategy).
    pub fn with_matrix(mut self, v: Value) -> Self {
        self.matrix = v;
        self
    }
    /// Sets the `strategy` context root (`Null` outside a matrix job).
    pub fn with_strategy(mut self, v: Value) -> Self {
        self.strategy = v;
        self
    }
    /// Sets the `steps` context root (conventionally an `Object` of
    /// `{id: {outputs, outcome, conclusion}}`).
    pub fn with_steps(mut self, v: Value) -> Self {
        self.steps = v;
        self
    }
    /// Sets the `runner` context root (conventionally an `Object` flat
    /// string map).
    pub fn with_runner(mut self, v: Value) -> Self {
        self.runner = v;
        self
    }
    /// Sets the `job` context root (conventionally an `Object`).
    pub fn with_job(mut self, v: Value) -> Self {
        self.job = v;
        self
    }
    /// Sets the `inputs` context root (conventionally an `Object` of typed
    /// values).
    pub fn with_inputs(mut self, v: Value) -> Self {
        self.inputs = v;
        self
    }
    /// Sets the [`RunStatus`] the status-check functions evaluate against.
    pub fn with_status(mut self, status: RunStatus) -> Self {
        self.status = status;
        self
    }

    /// Supplies a supplemental clock for deterministic `hashFiles()`
    /// deadline verification.
    ///
    /// The evaluator always uses the greater of real monotonic elapsed time
    /// and this clock's reported elapsed time. This seam can therefore only
    /// accelerate the non-configurable 120-second deadline; it cannot delay
    /// or disable it. The clock may be queried concurrently by the caller and
    /// filesystem worker.
    pub fn with_hash_files_clock(mut self, clock: Arc<dyn HashFilesClock>) -> Self {
        self.hash_files_clock = clock;
        self
    }

    /// The current [`RunStatus`] the status-check functions evaluate
    /// against.
    pub fn status(&self) -> RunStatus {
        self.status
    }

    /// The workspace root `hashFiles()` patterns are resolved against.
    pub fn workspace_root(&self) -> &Path {
        self.fs.workspace_root()
    }

    pub(crate) fn fs(&self) -> Arc<dyn HashFilesFs> {
        Arc::clone(&self.fs)
    }

    pub(crate) fn hash_files_clock(&self) -> Arc<dyn HashFilesClock> {
        Arc::clone(&self.hash_files_clock)
    }

    /// Resolves a context root name case-insensitively, matching the
    /// ordinal-ignore-case named-value registry in the Actions runner's
    /// [`ExpressionParser.cs`](https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionParser.cs).
    /// Returns
    /// `None` for anything outside the fixed eleven-name set — the caller
    /// (`parser`) is expected to have already rejected unrecognized root
    /// names at parse time, so this should never actually miss for an AST
    /// produced by [`crate::parse`], but stays total rather than panicking.
    pub(crate) fn resolve_root(&self, name: &str) -> Option<&Value> {
        if name.eq_ignore_ascii_case("github") {
            Some(&self.github)
        } else if name.eq_ignore_ascii_case("env") {
            Some(&self.env)
        } else if name.eq_ignore_ascii_case("vars") {
            Some(&self.vars)
        } else if name.eq_ignore_ascii_case("secrets") {
            Some(&self.secrets)
        } else if name.eq_ignore_ascii_case("needs") {
            Some(&self.needs)
        } else if name.eq_ignore_ascii_case("matrix") {
            Some(&self.matrix)
        } else if name.eq_ignore_ascii_case("strategy") {
            Some(&self.strategy)
        } else if name.eq_ignore_ascii_case("steps") {
            Some(&self.steps)
        } else if name.eq_ignore_ascii_case("runner") {
            Some(&self.runner)
        } else if name.eq_ignore_ascii_case("job") {
            Some(&self.job)
        } else if name.eq_ignore_ascii_case("inputs") {
            Some(&self.inputs)
        } else {
            None
        }
    }
}
