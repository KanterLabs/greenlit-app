#![forbid(unsafe_code)]

//! `greenlit-engine`: the planner.
//!
//! Turns a parsed workflow (from `greenlit-workflow`), a synthetic or real
//! trigger event, and resolved contexts (from `greenlit-expr`) into a
//! resolved [`ExecutionPlan`]: matrix expansion into concrete job instances
//! ([`matrix`]), `needs` DAG construction with cycle detection ([`graph`]),
//! statically-evaluable `if` conditions ([`condition`]) and job outputs
//! ([`outputs`]) partially evaluated without inventing values, and
//! supported-runner validation ([`runner`]). See `PHASE-1-engine-core.md`
//! for the full task list this crate implements; individual modules cite
//! GitHub documentation or runner source for parity-sensitive behavior.
//!
//! # Entry points for other crates
//!
//! - [`plan()`] — the single top-level function: `(workflow, event, options)
//!   -> ExecutionPlan`. This is what `litci plan`/`litci run` call.
//! - [`build_synthetic_event`] — builds the `github`/`inputs` context data
//!   for a simulated `push`/`pull_request`/`workflow_dispatch` trigger from
//!   local git metadata, since Phase 1 has no real webhook payload.
//! - [`ExecutionPlan`] derives [`serde::Serialize`] (as do all of its
//!   constituent types), so `litci plan --json` is just
//!   `serde_json::to_writer(stdout, &plan)` — see each type's doc comment
//!   for the exact stable JSON shape required by `PHASE-1-engine-core.md`.
//!
//! # A note on `unsafe`
//!
//! This crate has no `unsafe` blocks; `#![forbid(unsafe_code)]` above is a
//! hard compiler-enforced guarantee of that, per `AGENTS.md`'s repo hygiene
//! rule.

pub mod condition;
pub mod defer;
pub mod event;
pub mod git;
pub mod graph;
pub mod lints;
pub mod matrix;
pub mod outputs;
pub mod pass_through;
pub mod plan;
pub mod runner;

mod convert;
mod json_shape;
mod partial_eval;

pub use condition::{Condition, DeferredExpr, PlannedCond};
pub use defer::{DeferReason, StatusFn, StepStatusField};
pub use event::{EventError, EventKind, SyntheticEvent, build_synthetic_event};
pub use git::GitError;
pub use graph::{DependencyCycle, GraphError, JobGraph, JobId, JobIdx, build_graph};
pub use lints::{Lint, LintKind};
pub use matrix::{
    DEFAULT_MAX_MATRIX_LEGS, LegOrigin, MatrixError, MatrixLeg, MatrixValue, StrategyPlan,
};
pub use outputs::{JobOutputsPlan, PlannedOutput, PlannedValue};
pub use pass_through::{ContainerCredentials, ContainerPlan, EnvValue, LiteralValue};
pub use plan::{
    ExecutionPlan, JobPlan, LegPlan, PlanError, PlanOptions, StaticSkip, StepKind, StepPlan, plan,
};
pub use runner::{RunnerError, RunnerImage, SUPPORTED_RUNNER_LABELS, resolve_runner_label};
