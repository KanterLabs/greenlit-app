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

impl RunEvent {
    pub(super) fn protected_value_collision(
        &self,
        masker: &greenlit_engine::execution::Masker,
    ) -> bool {
        let collides = |value: &str| masker.apply(value) != value;
        match self {
            Self::RunStarted => false,
            Self::Preparation { phase, state, .. } => collides(phase) || collides(state),
            Self::JobStarted {
                job_id,
                instance_id,
                ..
            }
            | Self::JobSkipped {
                job_id,
                instance_id,
                ..
            } => collides(job_id) || collides(instance_id),
            Self::JobFinished {
                job_id,
                instance_id,
                conclusion,
                ..
            } => collides(job_id) || collides(instance_id) || collides(conclusion),
            Self::StepStarted {
                job_id,
                instance_id,
                event_id,
                step_id,
                kind,
                ..
            } => {
                collides(job_id)
                    || collides(instance_id)
                    || collides(event_id)
                    || step_id.as_deref().is_some_and(&collides)
                    || collides(kind)
            }
            Self::StepSkipped {
                job_id,
                instance_id,
                event_id,
                step_id,
                ..
            } => {
                collides(job_id)
                    || collides(instance_id)
                    || collides(event_id)
                    || step_id.as_deref().is_some_and(&collides)
            }
            Self::StepFinished {
                job_id,
                instance_id,
                event_id,
                step_id,
                outcome,
                conclusion,
                ..
            } => {
                collides(job_id)
                    || collides(instance_id)
                    || collides(event_id)
                    || step_id.as_deref().is_some_and(&collides)
                    || collides(outcome)
                    || collides(conclusion)
            }
            Self::Log {
                job_id,
                instance_id,
                step_event_id,
                ..
            } => {
                collides(job_id)
                    || collides(instance_id)
                    || step_event_id.as_deref().is_some_and(&collides)
            }
            Self::CacheSummary { store, .. } => collides(store),
            Self::CompatibilityFinding { code, scope, .. } => collides(code) || collides(scope),
            Self::RunFinished {
                conclusion,
                compatibility,
                assurance,
                evidence,
            } => {
                collides(conclusion)
                    || collides(compatibility)
                    || collides(assurance)
                    || collides(evidence)
            }
        }
    }

    pub(super) fn masked(self, masker: &greenlit_engine::execution::Masker) -> Self {
        let mask = |value: String| masker.apply(&value);
        let mask_option = |value: Option<String>| value.map(&mask);
        match self {
            Self::RunStarted => Self::RunStarted,
            Self::Preparation {
                phase,
                state,
                detail,
                current_bytes,
                total_bytes,
                cache_hit,
            } => Self::Preparation {
                phase,
                state,
                detail: mask_option(detail),
                current_bytes,
                total_bytes,
                cache_hit,
            },
            Self::JobStarted {
                job_id,
                instance_id,
                display,
            } => Self::JobStarted {
                job_id,
                instance_id,
                display: mask(display),
            },
            Self::JobSkipped {
                job_id,
                instance_id,
                display,
                reason,
            } => Self::JobSkipped {
                job_id,
                instance_id,
                display: mask(display),
                reason: mask(reason),
            },
            Self::JobFinished {
                job_id,
                instance_id,
                display,
                conclusion,
                duration_ms,
            } => Self::JobFinished {
                job_id,
                instance_id,
                display: mask(display),
                conclusion,
                duration_ms,
            },
            Self::StepStarted {
                job_id,
                instance_id,
                event_id,
                index,
                step_id,
                label,
                kind,
                reference,
            } => Self::StepStarted {
                job_id,
                instance_id,
                event_id,
                index,
                step_id,
                label: mask(label),
                kind,
                reference: mask_option(reference),
            },
            Self::StepSkipped {
                job_id,
                instance_id,
                event_id,
                index,
                step_id,
                label,
                reason,
            } => Self::StepSkipped {
                job_id,
                instance_id,
                event_id,
                index,
                step_id,
                label: mask(label),
                reason: mask(reason),
            },
            Self::StepFinished {
                job_id,
                instance_id,
                event_id,
                index,
                step_id,
                label,
                outcome,
                conclusion,
                duration_ms,
            } => Self::StepFinished {
                job_id,
                instance_id,
                event_id,
                index,
                step_id,
                label: mask(label),
                outcome,
                conclusion,
                duration_ms,
            },
            Self::Log {
                job_id,
                instance_id,
                step_event_id,
                text,
                partial,
            } => Self::Log {
                job_id,
                instance_id,
                step_event_id,
                text: mask(text),
                partial,
            },
            Self::CacheSummary {
                store,
                hits,
                misses,
            } => Self::CacheSummary {
                store,
                hits,
                misses,
            },
            Self::CompatibilityFinding {
                code,
                disposition,
                scope,
                reason,
            } => Self::CompatibilityFinding {
                code,
                disposition,
                scope,
                reason: mask(reason),
            },
            Self::RunFinished {
                conclusion,
                compatibility,
                assurance,
                evidence,
            } => Self::RunFinished {
                conclusion,
                compatibility,
                assurance,
                evidence,
            },
        }
    }
}
