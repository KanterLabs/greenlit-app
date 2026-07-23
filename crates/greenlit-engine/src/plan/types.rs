//! Serializable execution-plan types.

use indexmap::IndexMap;
use serde::Serialize;

use greenlit_expr::Value;
use greenlit_workflow::Span;

use crate::condition::Condition;
use crate::graph::JobId;
use crate::lints::Lint;
use crate::matrix::{DEFAULT_MAX_MATRIX_LEGS, StrategyPlan};
use crate::outputs::JobOutputsPlan;
use crate::pass_through::{ContainerPlan, EnvValue};
use crate::planned::Planned;
use crate::runner::RunnerPlan;

/// The fully resolved plan for one workflow file plus one synthetic event.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionPlan {
    /// Stable plan schema version.
    pub schema_version: u32,
    /// The simulated trigger's event name (`"push"`, `"pull_request"`,
    /// `"workflow_dispatch"`).
    pub event_name: String,
    /// Authored `run-name:`, resolved against its allowed workflow contexts.
    /// `None` means GitHub's event-specific default is used, including when
    /// an authored value folded to only whitespace. A deferred value must
    /// apply the same whitespace check after runtime evaluation.
    pub run_name: Option<Planned<String>>,
    /// Workflow-level `env:` entries. Runtime layering is represented
    /// explicitly as this outer layer, [`JobPlan::env`], then
    /// [`StepPlan::env`], with the more specific layer winning.
    pub env: IndexMap<String, EnvValue>,
    /// Workflow-level run defaults.
    pub defaults: RunDefaultsPlan,
    /// Workflow token permissions.
    pub permissions: Option<PermissionsPlan>,
    /// Every job, in the workflow file's declaration order.
    pub jobs: Vec<JobPlan>,
    /// The deterministic topological order — the order
    /// a scheduler would start jobs in, respecting `needs`.
    pub topo_order: Vec<JobId>,
    /// Every plan-time warning raised while building this plan.
    pub lints: Vec<Lint>,
}

/// One job, fully resolved as far as plan time allows.
#[derive(Debug, Clone, Serialize)]
pub struct JobPlan {
    /// The job id.
    pub id: JobId,
    /// Where this job is declared (the `jobs.<id>:` mapping key).
    #[serde(serialize_with = "crate::json_shape::serialize_span")]
    pub span: Span,
    /// Resolved display name for a non-matrix job (`name:` if given, else
    /// the id). For a matrix job this is the stable job id; each concrete
    /// instance's resolved display name is in [`LegPlan::name`].
    pub name: Planned<String>,
    /// Whether `name:` was omitted. A runtime-deferred matrix uses this to
    /// apply GitHub's default `job-id (matrix values...)` naming after its
    /// concrete values become available, without confusing an explicitly
    /// authored name that happens to equal the job id.
    pub name_is_default: bool,
    /// Direct dependencies, in `needs:` declaration order, deduplicated.
    pub needs: Vec<JobId>,
    /// `wave(j)`: `0` if `needs` is empty, else `1 + max` of its
    /// dependencies' waves.
    pub wave: u32,
    /// Planned runner image for a non-matrix job, or the runner template for
    /// a matrix whose expansion is runtime-deferred. `None` for a statically
    /// expanded matrix — see each [`LegPlan`] instead. A runtime-derived
    /// label carries no invented image and must be materialized before
    /// container provisioning.
    pub runner: Option<RunnerPlan>,
    /// `container:` configuration with each context-sensitive value folded
    /// or explicitly marked runtime-deferred.
    pub container: Option<ContainerPlan>,
    /// Service containers keyed by service id.
    pub services: IndexMap<String, ContainerPlan>,
    /// This job's own planned `env:` layer. It is applied after
    /// [`ExecutionPlan::env`] and before [`StepPlan::env`].
    pub env: IndexMap<String, EnvValue>,
    /// Effective workflow/job run defaults for this job.
    pub defaults: RunDefaultsPlan,
    /// Effective `GITHUB_TOKEN` permissions for this job. A job-level
    /// declaration replaces the workflow-level declaration rather than
    /// merging with it; `None` means GitHub's configured default applies.
    pub permissions: Option<PermissionsPlan>,
    /// This job's `if:`, resolved for a non-matrix job or retained once on a
    /// runtime-deferred matrix template. `None` for a statically expanded
    /// matrix — see each [`LegPlan`] instead — and when no `if:` was authored.
    pub condition: Option<Condition>,
    /// `true` iff `if:` (or its absence) contains no status-check function
    /// — the implicit `success()` gate applies. Identical across every leg
    /// of a matrix job (a purely syntactic property of the authored text).
    pub implicit_status_gate: bool,
    /// Statically decided skip for a non-matrix job or for the single
    /// template representing a runtime-deferred matrix.
    pub skip: Option<StaticSkip>,
    /// `strategy:`, resolved.
    pub strategy: StrategyPlan,
    /// One entry per [`StrategyPlan::legs`] leg, aligned by index. Empty for
    /// a non-matrix job, a zero-leg static matrix, or a matrix awaiting
    /// runtime materialization; [`StrategyPlan::matrix`] distinguishes them.
    pub legs: Vec<LegPlan>,
    /// `outputs:`, resolved as far as possible for a non-matrix job or a
    /// deferred matrix template. Empty for a statically expanded matrix;
    /// every concrete instance then retains its own plan in
    /// [`LegPlan::outputs`].
    pub outputs: JobOutputsPlan,
    /// The step sequence for a non-matrix job or deferred matrix template,
    /// in file order. Empty for a statically expanded matrix; each concrete
    /// instance then has independently planned steps in [`LegPlan::steps`].
    pub steps: Vec<StepPlan>,
}

/// The per-leg resolved data for a matrix job, aligned with
/// [`StrategyPlan::legs`] by index.
#[derive(Debug, Clone, Serialize)]
pub struct LegPlan {
    /// This instance's display name, resolved against its own `matrix`
    /// context.
    pub name: Planned<String>,
    /// This leg's planned runner image.
    pub runner: RunnerPlan,
    /// Matrix-sensitive job container configuration.
    pub container: Option<ContainerPlan>,
    /// Matrix-sensitive service container configuration.
    pub services: IndexMap<String, ContainerPlan>,
    /// Matrix-sensitive job `env:` layer.
    pub env: IndexMap<String, EnvValue>,
    /// Matrix-sensitive effective run defaults.
    pub defaults: RunDefaultsPlan,
    /// The job-level `if:` result copied to this instance after being
    /// evaluated once, before matrix application. GitHub does not expose
    /// `matrix` to `jobs.<job_id>.if`.
    pub condition: Option<Condition>,
    /// This leg's statically decided skip.
    pub skip: Option<StaticSkip>,
    /// This instance's job-output finalization plan, evaluated against its
    /// own matrix context and retaining runtime dependencies.
    pub outputs: JobOutputsPlan,
    /// This instance's independently planned step sequence.
    pub steps: Vec<StepPlan>,
}

/// A job/leg statically known to be skipped at plan time.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StaticSkip {
    /// Its own `if:` folded to `Static(false)`.
    ConditionFalse,
    /// Sound propagation: this job/leg's
    /// [`JobPlan::implicit_status_gate`] is `true`, and a needed job is
    /// itself fully, statically skipped — the implicit `success()` gate
    /// fails before this node's own condition (even a deferred one) is
    /// ever consulted. Never propagated through a node whose condition
    /// contains a status function.
    NeedSkipped {
        /// The skipped dependency.
        need: JobId,
    },
}

/// One step, fully resolved as far as plan time allows.
#[derive(Debug, Clone, Serialize)]
pub struct StepPlan {
    /// `id:`, if given.
    pub id: Option<String>,
    /// Display name with its authored source and static/deferred state.
    pub name: Option<Planned<String>>,
    /// This step's own `env:` entries (not merged with the outer layers —
    /// a caller combines [`ExecutionPlan::env`], [`JobPlan::env`], then
    /// this layer, with the more specific layer winning). Every value keeps
    /// its authored source and static/deferred evaluation state.
    pub env: IndexMap<String, EnvValue>,
    /// Explicit `working-directory:`, if present.
    pub working_directory: Option<Planned<String>>,
    /// `continue-on-error:`, if present.
    pub continue_on_error: Option<Planned<bool>>,
    /// `timeout-minutes:`, if present.
    pub timeout_minutes: Option<Planned<f64>>,
    /// This step's `if:`, resolved.
    pub condition: Option<Condition>,
    /// See [`JobPlan::implicit_status_gate`] — the step-level equivalent.
    pub implicit_status_gate: bool,
    /// Whether this is a `run:` or `uses:` step.
    pub kind: StepKind,
}

/// A step's action, per `PHASE-1-engine-core.md`: "step list with kind
/// (`run` | `uses`)".
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StepKind {
    /// A `run:` step.
    Run {
        /// The script body, folded against this job instance.
        script: Box<Planned<String>>,
        /// `shell:`, if given.
        shell: Option<Planned<String>>,
    },
    /// A `uses:` step.
    Uses {
        /// The action reference, verbatim (never an expression in valid
        /// GitHub syntax).
        reference: String,
        /// `with:` inputs with authored source and evaluation state.
        with: IndexMap<String, EnvValue>,
    },
}

/// Effective `defaults.run` configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunDefaultsPlan {
    /// Default shell.
    pub shell: Option<Planned<String>>,
    /// Default working directory.
    pub working_directory: Option<Planned<String>>,
}

/// Workflow- or job-level token permission declaration.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PermissionsPlan {
    /// Every supported scope is read-only.
    ReadAll,
    /// Every supported scope is writable.
    WriteAll,
    /// Explicit scope mapping; omitted scopes are `none` on GitHub.
    Scoped {
        /// Scope names in declaration order.
        scopes: IndexMap<String, PermissionLevelPlan>,
    },
}

/// One explicit token permission level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionLevelPlan {
    /// Read access.
    Read,
    /// Write access.
    Write,
    /// No access.
    None,
}

/// Inputs to [`crate::plan::plan`] beyond the workflow and event: local variable
/// overrides and the matrix-leg cap.
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// The already-resolved `vars` context (a flat object) — resolving the
    /// CLI-override/process-env/`.litci/vars` precedence chain is
    /// `greenlit-app`'s job (`PHASE-1-engine-core.md`'s greenlit-app
    /// section); this crate only evaluates against whatever map it is
    /// given.
    pub vars: Value,
    /// The matrix leg cap; defaults to
    /// [`DEFAULT_MAX_MATRIX_LEGS`].
    pub max_matrix_legs: usize,
}

impl Default for PlanOptions {
    fn default() -> Self {
        PlanOptions {
            vars: Value::object(vec![]),
            max_matrix_legs: DEFAULT_MAX_MATRIX_LEGS,
        }
    }
}
