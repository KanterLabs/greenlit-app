//! [`DeferReason`]: why a condition or output value could not be decided at
//! plan time. Shared between `crate::condition` and `crate::outputs` so every
//! deferred Phase 1 plan value has one stable representation.
//!
//! The classification implements `PHASE-1-engine-core.md`'s planner contract:
//! statically known values are evaluated, while runtime-only values such as
//! `needs.<id>.outputs.*`/`needs.<id>.result`,
//! `steps.<id>.outputs.*`/`.outcome`/`.conclusion`, the four status-check
//! functions, `hashFiles(...)`, `runner.*`, `job.*`, and `secrets.*` (at
//! step level; job-level `secrets.*` is a hard `PartialEvalError`
//! instead, per the docs' job-`if` context-availability rule). Synthetic
//! event-backed `github.*` properties remain static when locally knowable;
//! event-specific identities (such as a pull request's merge SHA), runner-,
//! run-, and API-backed properties defer explicitly rather than becoming
//! invented empty or placeholder values.

use crate::graph::JobId;

/// Which of `steps.<id>.outcome`/`steps.<id>.conclusion` was referenced —
/// both are runtime-only per-step status fields (docs: `outcome` is the
/// step's result *before* `continue-on-error` is applied; `conclusion` is
/// after).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StepStatusField {
    /// `steps.<id>.outcome`.
    Outcome,
    /// `steps.<id>.conclusion`.
    Conclusion,
}

/// Which status-check function was called — `success()`/`failure()`/
/// `cancelled()`/`always()` are all runtime-only. Even
/// though `always()` is a constant `true` in this crate's evaluator, its
/// entire *purpose* is suppressing the implicit status gate, which is a
/// scheduling decision, not a plan-time value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatusFn {
    /// `success()`.
    Success,
    /// `failure()`.
    Failure,
    /// `cancelled()`.
    Cancelled,
    /// `always()`.
    Always,
}

/// Why a condition or output value's residual expression could not be
/// resolved to a concrete value at plan time. Sorted, deduplicated in
/// [`crate::condition::DeferredExpr::defers_on`]/[`crate::outputs::PlannedValue::Deferred`]
/// to keep stable plan JSON deterministic.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeferReason {
    /// `needs.<job>.outputs.<output>`, or `needs.<job>.outputs` as a whole
    /// (`output: None`, e.g. inside `toJSON(needs.build.outputs)`).
    NeedsOutput {
        /// The referenced job.
        job: JobId,
        /// The specific output name, or `None` for a whole-object reference.
        output: Option<String>,
    },
    /// `needs.<job>.result`.
    NeedsResult {
        /// The referenced job.
        job: JobId,
    },
    /// `steps.<step>.outputs.<output>`, or the whole outputs object.
    StepOutput {
        /// The referenced step id.
        step: String,
        /// The specific output name, or `None` for a whole-object reference.
        output: Option<String>,
    },
    /// `steps.<step>.outcome` / `steps.<step>.conclusion`.
    StepStatus {
        /// The referenced step id.
        step: String,
        /// Which status field.
        field: StepStatusField,
    },
    /// A status-check function call.
    StatusFn(StatusFn),
    /// A `hashFiles(...)` call — reads the runner workspace, which does not
    /// exist at plan time.
    HashFiles,
    /// An `env.<name>` reference whose value may be changed or created by a
    /// preceding executable step through `GITHUB_ENV`, or whose indexed key
    /// cannot be reduced to one scalar name at planning time.
    DynamicEnv {
        /// The referenced env var name, or `"*"` for a dynamically indexed
        /// `env[...]` access whose selected name cannot be reduced to a
        /// single scalar value.
        name: String,
    },
    /// A property in the `github` context that is not available from the
    /// Phase 1 synthetic event, or the context as a whole (`property: None`).
    /// Runtime-only examples include `github.workspace` and
    /// `github.action`; API-backed examples include `github.actor_id`.
    GithubContext {
        /// The documented property name, or `None` for a whole-context
        /// reference such as `toJSON(github)`.
        property: Option<String>,
    },
    /// Any reference into the `runner` context — modeled wholesale as
    /// runtime-only, even for sub-fields that are in
    /// principle knowable earlier (e.g. `runner.os` — see
    /// `crate::partial_eval` module docs for why this crate does not special-case
    /// that).
    RunnerContext,
    /// Any reference into the `job` context.
    JobContext,
    /// A `matrix.*` reference while a prerequisite-produced matrix has not
    /// yet been expanded into concrete legs.
    MatrixContext,
    /// A `strategy.*` reference whose values depend on a matrix that has not
    /// yet been expanded.
    StrategyContext,
    /// A `secrets.*` reference outside job-level `if` (where it is instead a
    /// hard plan error — see `crate::partial_eval::PartialEvalError::SecretsForbiddenInJobCondition`).
    SecretsContext,
    /// A bare `needs`/`steps` context reference with no further indexing
    /// (e.g. `if: needs`) — degenerate in practice, but must still defer
    /// rather than silently resolve to an empty object. This is the total,
    /// defensive catch-all for the un-indexed case.
    NeedsContextWhole,
    /// See [`DeferReason::NeedsContextWhole`]; the `steps` equivalent.
    StepsContextWhole,
}
