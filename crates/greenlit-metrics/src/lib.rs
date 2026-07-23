#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! `greenlit-metrics`: local, instrumentation-first run metrics.
//!
//! A `tracing`-based span-timing layer over each pipeline stage (parse,
//! evaluate, plan, and later phases' stages), a versioned NDJSON writer that
//! appends one record per `plan`/`run` invocation to
//! `~/.litci/metrics/runs.ndjson`, and the reader `litci stats` uses to
//! render invocation history and stage trends. Strictly local: no network
//! code belongs in this crate, ever (`AGENTS.md` "Metrics"). See
//! `PHASE-1-engine-core.md` for the full task list this crate implements.
//!
//! This crate has no dependency on any other Greenlit crate — it only knows
//! about stage *names* (plain strings) and invocation *commands* (plain
//! strings), never about `greenlit-workflow`/`greenlit-expr`/`greenlit-engine`
//! types, so it stays usable by any future caller (the `litci` CLI, a
//! criterion bench in a sibling crate) without a dependency edge back onto
//! this one.
//!
//! ## Typical use (from a CLI command)
//!
//! ```
//! use greenlit_metrics::{Invocation, MetricsStore};
//!
//! # fn parse_workflow() -> Result<(), std::io::Error> { Ok(()) }
//! # fn evaluate_expressions() -> Result<(), std::io::Error> { Ok(()) }
//! # fn assemble_plan() -> Result<(), std::io::Error> { Ok(()) }
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let invocation = Invocation::start("plan");
//! invocation.with_timing_subscriber(|| -> Result<(), std::io::Error> {
//!     invocation.time_stage("parse", parse_workflow)?;
//!     invocation.time_stage("eval", evaluate_expressions)?;
//!     invocation.time_stage("plan", assemble_plan)?;
//!     Ok(())
//! })?;
//!
//! let record = invocation.finish();
//! // Render `record.stages` as a stderr table here, then append the record.
//! let store = MetricsStore::open_default()?;
//! store.append(&record)?;
//! # Ok(())
//! # }
//! ```
//!
//! `litci stats` instead calls only [`MetricsStore::read_recent`] — it must
//! never append a record for its own invocation. [`MetricsStore::read_all`]
//! remains available to callers that explicitly need the complete history.

mod error;
mod invocation;
mod record;
mod store;

pub use error::MetricsError;
pub use invocation::Invocation;
pub use record::{HitMissCounter, InvocationRecord, SCHEMA_VERSION, StageDuration, StepDuration};
pub use store::MetricsStore;
