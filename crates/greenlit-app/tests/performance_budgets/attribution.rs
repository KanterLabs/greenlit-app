//! Structured diagnostics for the native warm-start authority.

use serde_json::{Value, json};

use super::FIRST_STEP_MARKER;

const NAMED_PRE_STEP_STAGES: &[&str] = &[
    "source-freeze",
    "parse",
    "plan",
    "action-resolve",
    "detection",
    "runtime-fingerprint",
    "content-store-open",
    "image-resolve",
    "runner-resolve",
    "run-lock",
    "image-ensure",
    "container-boot",
    "overlay-setup",
];

pub(super) struct Sample<'a> {
    pub(super) number: usize,
    pub(super) invoked_at_unix_ms: u128,
    pub(super) first_user_step_unix_ms: u128,
    pub(super) workflow_ms: f64,
    pub(super) events: &'a [Value],
    pub(super) metrics: &'a Value,
}

pub(super) fn render(sample: Sample<'_>) -> String {
    let metrics_started_at = sample.metrics["started_at_unix_ms"]
        .as_u64()
        .map(u128::from)
        .expect("retained invocation start timestamp");
    let journal_started_at = unique_event_timestamp(sample.events, "run_started", |record| {
        record["type"] == "run_started"
    });
    let source_snapshot_at = preparation_timestamp(sample.events, "source snapshot", "finished");
    let workflow_at = preparation_timestamp(sample.events, "workflow", "finished");
    let execution_plan_at = preparation_timestamp(sample.events, "execution plan", "finished");
    let compatibility_at = preparation_timestamp(sample.events, "compatibility", "finished");
    let actions_at = preparation_timestamp(sample.events, "actions", "finished");
    let container_runtime_at =
        preparation_timestamp(sample.events, "container runtime", "finished");
    let containers_at = preparation_timestamp(sample.events, "containers", "finished");
    let runners_at = preparation_timestamp(sample.events, "runners", "finished");
    let run_lock_at = preparation_timestamp(sample.events, "RunLock", "finished");
    let job_started_at = unique_event_timestamp(sample.events, "job_started", |record| {
        record["type"] == "job_started"
    });
    let container_started_at = preparation_timestamp(sample.events, "container", "started");
    let container_finished_at = preparation_timestamp(sample.events, "container", "finished");
    let workspace_ready_at = preparation_timestamp(sample.events, "workspace", "ready");
    let step_started_at = unique_event_timestamp(sample.events, "first user step", |record| {
        record["type"] == "step_started"
            && record["index"].as_u64() == Some(0)
            && record["kind"] == "run"
    });
    let (embedded_marker_at, marker_journal_at) = marker_timestamps(sample.events);
    assert_eq!(
        embedded_marker_at, sample.first_user_step_unix_ms,
        "the attribution marker must match the performance authority marker"
    );

    let checkpoints = [
        ("process-invoked", sample.invoked_at_unix_ms),
        ("metrics-started", metrics_started_at),
        ("journal-run-started", journal_started_at),
        ("source-snapshot-finished", source_snapshot_at),
        ("workflow-finished", workflow_at),
        ("execution-plan-finished", execution_plan_at),
        ("compatibility-finished", compatibility_at),
        ("actions-finished", actions_at),
        ("container-runtime-finished", container_runtime_at),
        ("containers-finished", containers_at),
        ("runners-finished", runners_at),
        ("run-lock-finished", run_lock_at),
        ("job-started", job_started_at),
        ("container-started", container_started_at),
        ("container-finished", container_finished_at),
        ("workspace-ready", workspace_ready_at),
        ("step-started", step_started_at),
        ("first-user-code", embedded_marker_at),
        ("marker-journaled", marker_journal_at),
    ];
    let mut previous = checkpoints[0].1;
    let checkpoints = checkpoints
        .into_iter()
        .map(|(name, unix_ms)| {
            let delta_ms = unix_ms.checked_sub(previous).unwrap_or_else(|| {
                panic!(
                    "warm attribution checkpoint `{name}` at {unix_ms} predates its predecessor at {previous}"
                )
            });
            previous = unix_ms;
            json!({
                "name": name,
                "unix_ms": unix_ms,
                "delta_ms": delta_ms,
            })
        })
        .collect::<Vec<_>>();

    let stages = sample.metrics["stages"]
        .as_array()
        .expect("metrics stage array");
    let raw_ordered_stages = stages
        .iter()
        .map(|stage| {
            let name = stage["name"].as_str().expect("metrics stage name");
            let duration_ms = stage["duration_ms"]
                .as_f64()
                .expect("metrics stage duration");
            assert!(
                duration_ms.is_finite() && duration_ms >= 0.0,
                "stage `{name}` has invalid duration {duration_ms}"
            );
            json!({
                "name": name,
                "duration_ms": duration_ms,
            })
        })
        .collect::<Vec<_>>();
    let named_pre_step_ms = NAMED_PRE_STEP_STAGES
        .iter()
        .map(|name| required_stage_ms(stages, name))
        .sum::<f64>();
    let metrics_to_step_started_ms = step_started_at
        .checked_sub(metrics_started_at)
        .expect("first step start must not predate the metrics origin")
        as f64;
    let metrics_to_step_unattributed_ms = metrics_to_step_started_ms - named_pre_step_ms;
    let step_started_to_user_code_ms = embedded_marker_at
        .checked_sub(step_started_at)
        .expect("first user code must not predate its step-start event");
    let marker_to_journal_ms = marker_journal_at
        .checked_sub(embedded_marker_at)
        .expect("the marker journal record must not predate user code");

    let diagnostic = json!({
        "sample": sample.number,
        "invocation_to_first_user_step_ms": sample
            .first_user_step_unix_ms
            .checked_sub(sample.invoked_at_unix_ms)
            .expect("first user code must not predate process invocation"),
        "workflow_ms": sample.workflow_ms,
        "checkpoints": checkpoints,
        "raw_ordered_stages": raw_ordered_stages,
        "named_pre_step_ms": named_pre_step_ms,
        "metrics_to_step_unattributed_ms": metrics_to_step_unattributed_ms,
        "step_started_to_user_code_ms": step_started_to_user_code_ms,
        "marker_to_journal_ms": marker_to_journal_ms,
    });
    serde_json::to_string(&diagnostic).expect("warm attribution diagnostic must serialize")
}

fn preparation_timestamp(records: &[Value], phase: &str, state: &str) -> u128 {
    unique_event_timestamp(records, &format!("{phase}/{state}"), |record| {
        record["type"] == "preparation" && record["phase"] == phase && record["state"] == state
    })
}

fn unique_event_timestamp(
    records: &[Value],
    description: &str,
    predicate: impl Fn(&Value) -> bool,
) -> u128 {
    let matches = records
        .iter()
        .filter(|record| predicate(record))
        .map(|record| {
            record["timestamp_unix_ms"]
                .as_u64()
                .map(u128::from)
                .unwrap_or_else(|| panic!("{description} event has no Unix timestamp"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "warm run must retain exactly one `{description}` event"
    );
    matches[0]
}

fn marker_timestamps(records: &[Value]) -> (u128, u128) {
    let matches = records
        .iter()
        .filter(|record| record["type"] == "log")
        .filter_map(|record| {
            record["text"]
                .as_str()?
                .strip_prefix(FIRST_STEP_MARKER)
                .map(|timestamp| (record, timestamp))
        })
        .map(|(record, timestamp)| {
            let embedded = timestamp
                .parse::<u128>()
                .expect("first-user-step marker is a Unix millisecond timestamp");
            let journaled = record["timestamp_unix_ms"]
                .as_u64()
                .map(u128::from)
                .expect("first-user-step log has no Unix timestamp");
            (embedded, journaled)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "the first user step must retain exactly one timestamp marker"
    );
    matches[0]
}

fn required_stage_ms(stages: &[Value], name: &str) -> f64 {
    let matches = stages
        .iter()
        .filter(|stage| stage["name"] == name)
        .map(|stage| {
            stage["duration_ms"]
                .as_f64()
                .unwrap_or_else(|| panic!("stage `{name}` has no numeric duration"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "required pre-step stage `{name}` must appear exactly once"
    );
    matches[0]
}
