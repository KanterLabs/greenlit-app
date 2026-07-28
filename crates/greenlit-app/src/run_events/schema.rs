//! Stable persisted run-event schema.

use serde::{Deserialize, Serialize};

use greenlit_engine::FindingDisposition;

pub(super) const VERSION: u32 = 1;

/// One stable journal record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunEventRecord {
    pub(crate) schema_version: u32,
    pub(crate) sequence: u64,
    pub(crate) timestamp_unix_ms: u64,
    pub(crate) elapsed_ms: u64,
    pub(crate) run_id: String,
    #[serde(flatten)]
    pub(crate) event: RunEvent,
}

/// Stable event payload persisted in `events.ndjson`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum RunEvent {
    RunStarted,
    Preparation {
        phase: String,
        state: String,
        detail: Option<String>,
        current_bytes: Option<u64>,
        total_bytes: Option<u64>,
        cache_hit: Option<bool>,
    },
    JobStarted {
        job_id: String,
        instance_id: String,
        display: String,
    },
    JobSkipped {
        job_id: String,
        instance_id: String,
        display: String,
        reason: String,
    },
    JobFinished {
        job_id: String,
        instance_id: String,
        display: String,
        conclusion: String,
        duration_ms: u64,
    },
    StepStarted {
        job_id: String,
        instance_id: String,
        event_id: String,
        index: usize,
        step_id: Option<String>,
        label: String,
        kind: String,
        reference: Option<String>,
    },
    StepSkipped {
        job_id: String,
        instance_id: String,
        event_id: String,
        index: usize,
        step_id: Option<String>,
        label: String,
        reason: String,
    },
    StepFinished {
        job_id: String,
        instance_id: String,
        event_id: String,
        index: usize,
        step_id: Option<String>,
        label: String,
        outcome: String,
        conclusion: String,
        duration_ms: u64,
    },
    Log {
        job_id: String,
        instance_id: String,
        step_event_id: Option<String>,
        text: String,
        partial: bool,
    },
    CacheSummary {
        store: String,
        hits: u64,
        misses: u64,
    },
    CompatibilityFinding {
        code: String,
        disposition: FindingDisposition,
        scope: String,
        reason: String,
    },
    RunFinished {
        conclusion: String,
        compatibility: String,
        assurance: String,
        evidence: String,
    },
}
