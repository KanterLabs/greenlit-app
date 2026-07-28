//! The versioned NDJSON record schema.
//!
//! `AGENTS.md` ("Metrics") requires the record schema to be "versioned from
//! day one" and `TESTING.md` declares this schema one of the two surfaces
//! (alongside `litci plan --json`) allowed a snapshot test. [`SCHEMA_VERSION`]
//! is bumped and a migration note added here whenever a field is added,
//! renamed, or removed; existing fields are never repurposed.
//!
//! # Migration notes
//!
//! **1 → 2 (Phase 4).** [`HitMissCounter`] gained `bytes`, so a counter can
//! report how much data it moved and not only how often it was consulted —
//! `PHASE-4-environment.md` asks for "cache-shim hit/miss **and bytes
//! served**", and hits alone cannot express that.
//!
//! [`crate::MetricsStore`] rejects any record whose version differs from
//! [`SCHEMA_VERSION`], so this bump makes records written under version 1
//! unreadable to `litci stats`; they are preserved on disk but skipped. That
//! is deliberate rather than overlooked: `AGENTS.md` states "there is no
//! legacy command or data-path compatibility requirement because no version
//! has shipped", so a reader that silently upgraded old records would be
//! carrying migration machinery for a population of zero.

use serde::{Deserialize, Serialize};

/// Current version of [`InvocationRecord`]'s on-disk shape.
///
/// Every record written by [`crate::MetricsStore::append`] carries this
/// value verbatim in its `schema_version` field so a future reader (a later
/// phase, or `litci stats` after a schema change) can tell which shape a
/// given NDJSON line was written under.
pub const SCHEMA_VERSION: u32 = 2;

/// One completed pipeline stage's wall-clock timing, in the order it was
/// recorded.
///
/// Phase 1's stages are `parse`, `eval`, and `plan` (see
/// `PHASE-1-engine-core.md`), but this crate does not hardcode that list —
/// `name` is whatever string the caller passed to
/// [`crate::Invocation::time_stage`] or
/// [`crate::Invocation::time_stage_async`], so later phases can add stages
/// (image ensure, container boot, step exec, action resolve, cache lookup,
/// per `AGENTS.md`'s Metrics section) without a `greenlit-metrics` code
/// change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageDuration {
    /// The stage name, e.g. `"parse"`, `"eval"`, `"plan"`.
    pub name: String,
    /// Wall-clock duration of the stage, in fractional milliseconds.
    pub duration_ms: f64,
}

/// One executed workflow step's duration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepDuration {
    /// Stable job-instance identifier.
    pub job: String,
    /// Stable step id or display label.
    pub step: String,
    /// Wall-clock duration in fractional milliseconds.
    pub duration_ms: f64,
}

/// Hit/miss totals for one named local lookup/cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitMissCounter {
    /// Counter name, such as `action-fetch` or `toolcache`.
    pub name: String,
    /// Successful local hits.
    pub hits: u64,
    /// Misses requiring slower work.
    pub misses: u64,
    /// Bytes this counter's subject moved, when that is meaningful.
    ///
    /// Zero for counters that only ever answer "was it there?" — an action
    /// fetch either hit the store or did not. The cache shim reports what it
    /// actually served, which is the number that tells a user whether a
    /// cache is earning its disk.
    pub bytes: u64,
}

/// One appended record: the complete stage-timing breakdown of a single
/// `litci plan` or `litci run` invocation.
///
/// This is the stable, versioned unit written to
/// `~/.litci/metrics/runs.ndjson`, one per line, newest last. Constructed via
/// [`crate::Invocation::finish`]; read back via
/// [`crate::MetricsStore::read_recent`] or [`crate::MetricsStore::read_all`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvocationRecord {
    /// Schema version this record was written under. See [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The CLI (sub)command this invocation was, e.g. `"plan"` or `"run"`.
    ///
    /// A plain string rather than a closed enum: `greenlit-metrics` has no
    /// dependency on `greenlit-app` and should not need a release whenever
    /// the CLI's command set grows. `litci stats` never appends a record for
    /// itself (`AGENTS.md` Metrics section, `PHASE-1-engine-core.md`).
    pub command: String,
    /// Milliseconds since the Unix epoch when the invocation began.
    pub started_at_unix_ms: u128,
    /// Total wall-clock duration of the whole invocation, in fractional
    /// milliseconds — the "totals" `AGENTS.md`'s Metrics section calls for.
    pub total_duration_ms: f64,
    /// Per-stage timings, in the order each stage was opened.
    pub stages: Vec<StageDuration>,
    /// Per-step timings; empty for `plan` invocations.
    pub steps: Vec<StepDuration>,
    /// Named hit/miss counters; empty until a lookup occurs.
    pub hit_miss: Vec<HitMissCounter>,
}
