//! Whole-run Phase 10 performance gate on a native Linux x86_64 Docker host.

pub mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use support::Sandbox;

const WARM_SAMPLES: usize = 20;
const FIRST_STEP_MARKER: &str = "greenlit-first-user-step-unix-ms=";

fn fixture_root() -> PathBuf {
    std::fs::canonicalize(format!(
        "{}/../../fixtures/performance",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("performance fixture exists")
}

fn copy_fixture(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create fixture destination");
    for entry in std::fs::read_dir(src).expect("read fixture") {
        let entry = entry.expect("fixture entry");
        let destination = dst.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            copy_fixture(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).expect("copy fixture file");
        }
    }
}

fn docker_reachable() -> bool {
    use greenlit_runtime::DockerEngine;
    use greenlit_runtime::detect::Endpoint;
    use greenlit_runtime::engine::ContainerEngine;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let Ok(engine) = DockerEngine::connect(&Endpoint::DockerSocket) else {
            return false;
        };
        engine
            .image_exists("greenlit/probe:definitely-absent")
            .await
            .is_ok()
    })
}

fn unix_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("benchmark host clock is after the Unix epoch")
        .as_millis()
}

fn run_directories(root: &Path) -> BTreeSet<PathBuf> {
    std::fs::read_dir(root)
        .expect("read run evidence directory")
        .map(|entry| entry.expect("read run evidence entry").path())
        .collect()
}

fn one_new_run(root: &Path, before: &BTreeSet<PathBuf>) -> PathBuf {
    let after = run_directories(root);
    let created = after.difference(before).cloned().collect::<Vec<_>>();
    assert_eq!(
        created.len(),
        1,
        "one litci invocation must retain exactly one run directory"
    );
    created[0].clone()
}

fn journal_records(run: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(run.join("events.ndjson"))
        .expect("read retained event journal")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event journal record is JSON"))
        .collect()
}

fn first_user_step_unix_ms(records: &[serde_json::Value]) -> u128 {
    let markers = records
        .iter()
        .filter(|record| record["type"] == "log")
        .filter_map(|record| record["text"].as_str())
        .filter_map(|text| text.strip_prefix(FIRST_STEP_MARKER))
        .map(|timestamp| {
            timestamp
                .parse::<u128>()
                .expect("first-user-step marker is a Unix millisecond timestamp")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        markers.len(),
        1,
        "the first user step must retain exactly one timestamp marker"
    );
    markers[0]
}

fn assert_zero_setup_downloads(records: &[serde_json::Value]) {
    let mut verified_resolutions = 0;
    for record in records
        .iter()
        .filter(|record| record["type"] == "preparation")
    {
        let phase = record["phase"].as_str().expect("preparation phase");
        let state = record["state"].as_str().expect("preparation state");
        if state == "resolved" && record["cache_hit"].is_boolean() {
            assert_eq!(
                record["cache_hit"], true,
                "warm content metadata was not resolved from verified local content"
            );
            verified_resolutions += 1;
        }
        if phase == "runner content" {
            assert!(
                !matches!(state, "started" | "progress" | "finished"),
                "warm runner setup retained a download event: {record}"
            );
            if let Some(bytes) = record["current_bytes"].as_u64() {
                assert_eq!(
                    bytes, 0,
                    "warm runner setup retained {bytes} downloaded bytes"
                );
            }
        }
        assert!(
            phase != "runner build",
            "warm runner setup rebuilt content instead of reusing it: {record}"
        );
    }
    assert!(
        verified_resolutions > 0,
        "warm run retained no verified content resolution"
    );
}

fn assert_daemon_quarantined(sandbox: &Sandbox) {
    assert!(
        !sandbox.home().join(".litci/daemon").exists(),
        "a normal Phase 12 run started or prepared daemon state"
    );
}

#[test]
fn native_warm_budgets_and_zero_setup_downloads_are_enforced() {
    assert!(
        std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64",
        "the Phase 10 benchmark host must be native Linux x86_64"
    );
    assert!(
        docker_reachable(),
        "the Phase 10 benchmark job must provide a reachable Docker daemon"
    );

    let sandbox = Sandbox::new();
    copy_fixture(&fixture_root(), sandbox.root());
    sandbox.init_git();

    let cold = sandbox.run_with_env(&["run", "--no-input", "--allow-degraded"], &[]);
    assert!(
        cold.status.success(),
        "cold setup failed\nstdout:\n{}\nstderr:\n{}",
        support::stdout_text(&cold),
        support::stderr_text(&cold)
    );
    let cold_stdout = support::stdout_text(&cold);
    assert!(
        !cold_stdout.contains("successful body retained only in the journal")
            && !cold_stdout.contains("workflow fake marker"),
        "compact output must hide successful workflow bodies: {cold_stdout}"
    );
    let runs = sandbox.home().join(".litci/runs");
    let cold_run = std::fs::read_dir(&runs)
        .expect("run evidence directory")
        .filter_map(Result::ok)
        .max_by_key(|entry| entry.file_name())
        .expect("cold run directory");
    let journal =
        std::fs::read_to_string(cold_run.path().join("events.ndjson")).expect("event journal");
    assert!(journal.contains("\"type\":\"step_finished\""));
    assert!(journal.contains("successful body retained only in the journal"));
    assert!(journal.contains("workflow fake marker: OK forged success"));
    assert!(journal.contains("\"type\":\"run_finished\""));
    support::assert_run_resources_removed(&cold_run.path());
    assert_daemon_quarantined(&sandbox);

    let mut invocation_to_step_ms = Vec::with_capacity(WARM_SAMPLES);
    let mut invocation_windows = Vec::with_capacity(WARM_SAMPLES);
    let mut workflow_ms = Vec::with_capacity(WARM_SAMPLES);
    for _ in 0..WARM_SAMPLES {
        let before = run_directories(&runs);
        let invoked_at_unix_ms = unix_ms_now();
        let workflow_started = Instant::now();
        let output = sandbox.run_with_env(&["run", "--no-input", "--allow-degraded"], &[]);
        let workflow_duration_ms = workflow_started.elapsed().as_secs_f64() * 1000.0;
        assert!(
            output.status.success(),
            "warm run failed\nstdout:\n{}\nstderr:\n{}",
            support::stdout_text(&output),
            support::stderr_text(&output)
        );
        let run = one_new_run(&runs, &before);
        let events = journal_records(&run);
        let first_step_unix_ms = first_user_step_unix_ms(&events);
        assert!(
            first_step_unix_ms >= invoked_at_unix_ms,
            "first user step timestamp predates the litci process invocation"
        );
        invocation_to_step_ms.push((first_step_unix_ms - invoked_at_unix_ms) as f64);
        invocation_windows.push((invoked_at_unix_ms, first_step_unix_ms));
        workflow_ms.push(workflow_duration_ms);
        assert_zero_setup_downloads(&events);
        support::assert_run_resources_removed(&run);
        assert_daemon_quarantined(&sandbox);
    }

    let records = std::fs::read_to_string(sandbox.metrics_file()).expect("metrics records");
    let mut records = records
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("metrics JSON"))
        .filter(|record| record["command"] == "run")
        .collect::<Vec<_>>();
    assert_eq!(records.len(), WARM_SAMPLES + 1);
    records.remove(0);

    let mut setup_stage_ms = records
        .iter()
        .map(|record| {
            let setup = ["image-ensure", "container-boot", "overlay-setup"]
                .into_iter()
                .map(|name| stage_ms(record, name))
                .sum::<f64>();
            let _exec = stage_ms(record, "exec");
            setup
        })
        .collect::<Vec<_>>();
    for (record, (invoked_at_unix_ms, first_step_unix_ms)) in
        records.iter().zip(invocation_windows.iter())
    {
        let started_at_unix_ms = record["started_at_unix_ms"]
            .as_u64()
            .expect("retained invocation start timestamp") as u128;
        assert!(
            started_at_unix_ms >= *invoked_at_unix_ms && started_at_unix_ms <= *first_step_unix_ms,
            "retained metrics start is outside the measured invocation-to-step window"
        );
        assert!(
            record["total_duration_ms"]
                .as_f64()
                .is_some_and(|duration| duration.is_finite() && duration > 0.0),
            "metrics total duration is missing or invalid"
        );
    }
    invocation_to_step_ms.sort_by(f64::total_cmp);
    setup_stage_ms.sort_by(f64::total_cmp);
    workflow_ms.sort_by(f64::total_cmp);
    let percentile_index = (WARM_SAMPLES * 95).div_ceil(100) - 1;
    eprintln!(
        "warm budgets: invocation-to-first-user-step p95 {:.2} ms; setup stages p95 {:.2} ms; \
         workflow p95 {:.2} ms; retained setup downloads 0",
        invocation_to_step_ms[percentile_index],
        setup_stage_ms[percentile_index],
        workflow_ms[percentile_index]
    );
    assert!(
        invocation_to_step_ms[percentile_index] < 2_000.0,
        "warm invocation-to-first-user-step p95 was {:.2} ms, budget is < 2000 ms",
        invocation_to_step_ms[percentile_index]
    );
    assert!(
        workflow_ms[percentile_index] < 30_000.0,
        "warm workflow p95 was {:.2} ms, budget is < 30000 ms",
        workflow_ms[percentile_index]
    );

    let before_failure = run_directories(&runs);
    let failure = sandbox.run_with_env(
        &[
            "run",
            "--no-input",
            "--allow-degraded",
            "--event",
            "workflow_dispatch",
        ],
        &[],
    );
    assert!(!failure.status.success(), "failure fixture must fail");
    let failure_stdout = support::stdout_text(&failure);
    assert!(
        !failure_stdout.contains("failure-line-000"),
        "compact failure output exceeded the 200-line tail: {failure_stdout}"
    );
    assert!(
        failure_stdout.contains("failure-line-204"),
        "failure tail omitted its final line: {failure_stdout}"
    );
    assert!(failure_stdout.contains("full log: litci logs "));
    assert!(failure_stdout.contains("--job test-000 --step step-1"));
    let failure_run = one_new_run(&runs, &before_failure);
    support::assert_run_resources_removed(&failure_run);
    assert_daemon_quarantined(&sandbox);

    let before_jsonl = run_directories(&runs);
    let jsonl = sandbox.run_with_env(
        &["run", "--no-input", "--allow-degraded", "--format", "jsonl"],
        &[],
    );
    assert!(
        jsonl.status.success(),
        "JSONL run failed: {}",
        support::stderr_text(&jsonl)
    );
    let jsonl_stdout = support::stdout_text(&jsonl);
    let mut run_id = None;
    for line in jsonl_stdout.lines() {
        let event: serde_json::Value =
            serde_json::from_str(line).expect("JSONL stdout contains only events");
        let event_run_id = event["run_id"].as_str().expect("event run id");
        match &run_id {
            Some(expected) => assert_eq!(event_run_id, expected),
            None => run_id = Some(event_run_id.to_string()),
        }
    }
    let run_id = run_id.expect("JSONL emitted a run_started event");
    let jsonl_run = one_new_run(&runs, &before_jsonl);
    assert_eq!(
        jsonl_run.file_name().and_then(|name| name.to_str()),
        Some(run_id.as_str()),
        "JSONL stream and retained run directory must use the same identity"
    );
    let persisted =
        std::fs::read_to_string(jsonl_run.join("events.ndjson")).expect("JSONL run journal");
    assert_eq!(jsonl_stdout, persisted);
    support::assert_run_resources_removed(&jsonl_run);
    assert_daemon_quarantined(&sandbox);
}

fn stage_ms(record: &serde_json::Value, name: &str) -> f64 {
    let duration = record["stages"]
        .as_array()
        .expect("stage array")
        .iter()
        .find(|stage| stage["name"] == name)
        .and_then(|stage| stage["duration_ms"].as_f64())
        .unwrap_or_else(|| panic!("required stage `{name}` is absent from the metrics record"));
    assert!(
        duration.is_finite() && duration >= 0.0,
        "stage `{name}` has invalid duration {duration}"
    );
    duration
}
