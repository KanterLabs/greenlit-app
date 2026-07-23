//! The versioned NDJSON record schema.
//!
//! `AGENTS.md` ("Metrics") requires the record schema to be "versioned from
//! day one" and `TESTING.md` declares this schema one of the two surfaces
//! (alongside `litci plan --json`) allowed a snapshot test. [`SCHEMA_VERSION`]
//! is bumped and a migration note added here whenever a field is added,
//! renamed, or removed; existing fields are never repurposed.

use serde::{Deserialize, Serialize};

/// Current version of [`InvocationRecord`]'s on-disk shape.
///
/// Every record written by [`crate::MetricsStore::append`] carries this
/// value verbatim in its `schema_version` field so a future reader (a later
/// phase, or `litci stats` after a schema change) can tell which shape a
/// given NDJSON line was written under.
pub const SCHEMA_VERSION: u32 = 1;

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

#[cfg(test)]
mod tests {
    use super::*;

    // This is the one declared stable snapshot surface for this crate
    // (TESTING.md: "No snapshot tests except ... the metrics record
    // schema"). It pins the exact serialized field set, order, and types of
    // `InvocationRecord`; a deliberate schema change updates both this
    // expected string and `SCHEMA_VERSION`, never one without the other.
    #[test]
    fn invocation_record_json_shape_is_pinned() {
        let record = InvocationRecord {
            schema_version: SCHEMA_VERSION,
            command: "plan".to_string(),
            started_at_unix_ms: 1_700_000_000_000,
            total_duration_ms: 12.5,
            stages: vec![
                StageDuration {
                    name: "parse".to_string(),
                    duration_ms: 1.0,
                },
                StageDuration {
                    name: "eval".to_string(),
                    duration_ms: 2.5,
                },
                StageDuration {
                    name: "plan".to_string(),
                    duration_ms: 9.0,
                },
            ],
            steps: Vec::new(),
            hit_miss: Vec::new(),
        };

        let json = serde_json::to_string(&record).expect("record must serialize");
        assert_eq!(
            json,
            "{\"schema_version\":1,\"command\":\"plan\",\"started_at_unix_ms\":1700000000000,\
             \"total_duration_ms\":12.5,\"stages\":[{\"name\":\"parse\",\"duration_ms\":1.0},\
             {\"name\":\"eval\",\"duration_ms\":2.5},{\"name\":\"plan\",\"duration_ms\":9.0}],\
             \"steps\":[],\"hit_miss\":[]}"
        );

        // Round-trips back to an equal value, independent of the pinned
        // string above (guards against the string and the type drifting
        // together in a way that still "matches").
        let parsed: InvocationRecord = serde_json::from_str(&json).expect("record must parse");
        assert_eq!(parsed, record);
    }
}
