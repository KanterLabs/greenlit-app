//! The typed evaluation context: one [`Value`] per legal `${{ }}` root name.
//!
//! Source for the root name set and shape notes: design memo §5 ("Contexts:
//! shapes, typing, missing-key behavior"), cross-referenced against the
//! [Contexts reference](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts).
//! Per the Phase 1 task list this crate implements exactly the ten roots
//! `github`, `env`, `vars`, `secrets`, `needs`, `matrix`, `steps`, `runner`,
//! `job`, `inputs` — not `strategy` or the reusable-workflow `jobs` context,
//! which the design memo also discusses but which are out of this phase's
//! explicit scope.
//!
//! **Per-key context availability is not modeled here.** Real GitHub
//! additionally restricts which of these ten roots are legal in a given
//! workflow key (the "Context availability" table — e.g. `secrets` is not
//! legal in `jobs.<id>.if`); this crate treats all ten roots as always
//! resolvable and leaves any narrower per-key allow-list to
//! `greenlit-workflow`/`greenlit-engine`, which can walk the public
//! [`crate::ast::Expr`] tree themselves to find referenced root names.

use std::path::Path;
use std::rc::Rc;

use crate::functions::hash_files::HashFilesFs;
use crate::value::Value;

/// The fixed, case-insensitive set of context root names this crate
/// recognizes. Order matches the design memo's context table.
pub(crate) const ROOT_NAMES: [&str; 10] = [
    "github", "env", "vars", "secrets", "needs", "matrix", "steps", "runner", "job", "inputs",
];

/// The rolling job/step status status-check functions evaluate against.
///
/// Source: design memo §4 ("Status functions and the implicit status
/// check") — the runner keeps a rolling status (`JobContext.Status`,
/// initially `Success`; becomes `Failure` when a step fails without
/// `continue-on-error`; becomes `Cancelled` on cancellation). Computing
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
/// injected [`HashFilesFs`] for `hashFiles()`.
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
    steps: Value,
    runner: Value,
    job: Value,
    inputs: Value,
    status: RunStatus,
    fs: Rc<dyn HashFilesFs>,
}

impl Context {
    /// Builds a context with every root defaulted to an empty object
    /// (`matrix` to `Null`, since a job without a matrix strategy has no
    /// matrix context data) and [`RunStatus::Success`], backed by `fs` for
    /// `hashFiles()`.
    pub fn new(fs: Rc<dyn HashFilesFs>) -> Self {
        Context {
            github: Value::object(vec![]),
            env: Value::object(vec![]),
            vars: Value::object(vec![]),
            secrets: Value::object(vec![]),
            needs: Value::object(vec![]),
            matrix: Value::Null,
            steps: Value::object(vec![]),
            runner: Value::object(vec![]),
            job: Value::object(vec![]),
            inputs: Value::object(vec![]),
            status: RunStatus::default(),
            fs,
        }
    }

    /// Sets the `github` context root (conventionally an `Object`).
    pub fn with_github(mut self, v: Value) -> Self {
        self.github = v;
        self
    }
    /// Sets the `env` context root (conventionally an `Object` flat string
    /// map).
    pub fn with_env(mut self, v: Value) -> Self {
        self.env = v;
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

    /// The current [`RunStatus`] the status-check functions evaluate
    /// against.
    pub fn status(&self) -> RunStatus {
        self.status
    }

    /// The workspace root `hashFiles()` patterns are resolved against.
    pub fn workspace_root(&self) -> &Path {
        self.fs.workspace_root()
    }

    pub(crate) fn fs(&self) -> &dyn HashFilesFs {
        self.fs.as_ref()
    }

    /// Resolves a context root name case-insensitively (design memo §1.1:
    /// "root named-value ... names resolve case-insensitively"). Returns
    /// `None` for anything outside the fixed ten-name set — the caller
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
