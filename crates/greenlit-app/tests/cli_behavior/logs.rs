use crate::support::{Sandbox, stderr_text, stdout_text};

const RUN_ID: &str = "00000000000000000000000000000002-00000001-0000";

fn journal() -> String {
    [
        serde_json::json!({
            "schema_version": 1,
            "sequence": 1,
            "timestamp_unix_ms": 10,
            "elapsed_ms": 1,
            "run_id": RUN_ID,
            "type": "step_started",
            "job_id": "build",
            "instance_id": "build-000",
            "event_id": "step-0",
            "index": 0,
            "step_id": "compile",
            "label": "Compile",
            "kind": "run",
            "reference": null
        }),
        serde_json::json!({
            "schema_version": 1,
            "sequence": 2,
            "timestamp_unix_ms": 11,
            "elapsed_ms": 2,
            "run_id": RUN_ID,
            "type": "log",
            "job_id": "build",
            "instance_id": "build-000",
            "step_event_id": "step-0",
            "text": "first line",
            "partial": false
        }),
        serde_json::json!({
            "schema_version": 1,
            "sequence": 3,
            "timestamp_unix_ms": 12,
            "elapsed_ms": 3,
            "run_id": RUN_ID,
            "type": "log",
            "job_id": "build",
            "instance_id": "build-000",
            "step_event_id": "step-0",
            "text": "second line",
            "partial": false
        }),
        serde_json::json!({
            "schema_version": 1,
            "sequence": 4,
            "timestamp_unix_ms": 13,
            "elapsed_ms": 4,
            "run_id": RUN_ID,
            "type": "run_finished",
            "conclusion": "Passed",
            "compatibility": "Degraded",
            "assurance": "Local",
            "evidence": RUN_ID
        }),
    ]
    .into_iter()
    .map(|event| format!("{event}\n"))
    .collect()
}

fn sandbox_with_journal() -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write_home(&format!(".litci/runs/{RUN_ID}/events.ndjson"), &journal());
    sandbox
}

#[test]
fn logs_replays_latest_journal_with_job_step_and_tail_filters() {
    let sandbox = sandbox_with_journal();
    let output = sandbox.run(&["logs", "--job", "build", "--step", "compile", "--tail", "1"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let stdout = stdout_text(&output);
    assert!(!stdout.contains("first line"), "{stdout}");
    assert!(
        stdout.contains("[build-000 > step-0] second line"),
        "{stdout}"
    );
}

#[test]
fn logs_jsonl_returns_original_matching_records() {
    let sandbox = sandbox_with_journal();
    let output = sandbox.run(&["logs", RUN_ID, "--format", "jsonl"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let stdout = stdout_text(&output);
    assert_eq!(stdout.lines().count(), 2);
    for line in stdout.lines() {
        let event: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|error| panic!("valid event: {error}"));
        assert_eq!(event["type"], "log");
        assert_eq!(event["run_id"], RUN_ID);
    }
}

#[test]
fn logs_rejects_runs_without_an_event_journal() {
    let sandbox = Sandbox::new();
    sandbox.write_home(
        &format!(".litci/runs/{RUN_ID}/result.json"),
        r#"{"schema_version":1}"#,
    );
    let output = sandbox.run(&["logs", RUN_ID]);
    assert!(!output.status.success());
    assert!(stderr_text(&output).contains("has no structured log journal"));
}
