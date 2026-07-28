use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use greenlit_store::cas::{CasStore, RunCatalogState};

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
            "compatibility": "Supported",
            "assurance": "Local",
            "evidence": RUN_ID
        }),
    ]
    .into_iter()
    .map(|event| format!("{event}\n"))
    .collect()
}

fn private_run_directory(sandbox: &Sandbox) -> PathBuf {
    let runs = sandbox.home().join(".litci/runs");
    let directory = runs.join(RUN_ID);
    fs::create_dir_all(&directory).expect("private run directory should be created");
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o700))
        .expect("runs root should be private");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .expect("run directory should be private");
    directory
}

fn write_private(path: &Path, contents: &str) {
    fs::write(path, contents).expect("retained artifact should be written");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .expect("retained artifact should be private");
}

fn sandbox_with_journal() -> Sandbox {
    let sandbox = Sandbox::new();
    let directory = private_run_directory(&sandbox);
    write_private(&directory.join("events.ndjson"), &journal());
    sandbox
}

fn seed_matching_result_and_trace(sandbox: &Sandbox) {
    let directory = sandbox.home().join(".litci/runs").join(RUN_ID);
    write_private(
        &directory.join("result.json"),
        r#"{"schema_version":1,"conclusion":"passed","compatibility":"supported","assurance":"local","reasons":[]}"#,
    );
    write_private(
        &directory.join("trace.ndjson"),
        "{\"schema_version\":1,\"sequence\":1,\"event\":\"run_completed\",\"attributes\":{\"assurance\":\"Local\",\"compatibility\":\"Supported\",\"conclusion\":\"Passed\"}}\n",
    );
}

fn store(sandbox: &Sandbox) -> CasStore {
    CasStore::open(CasStore::default_path_under(sandbox.home())).expect("catalog should open")
}

fn spawn_follow(sandbox: &Sandbox) -> Child {
    let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_litci"));
    command
        .args(["logs", RUN_ID, "--follow"])
        .current_dir(sandbox.root())
        .env_clear()
        .env("PATH", path)
        .env("HOME", sandbox.home())
        .env("XDG_CONFIG_HOME", sandbox.home().join(".config"))
        .env("LITCI_TEST_NO_KEYRING", "1")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn().expect("logs --follow should spawn")
}

fn wait_for_output(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child
            .try_wait()
            .expect("follow process should remain observable")
            .is_some()
        {
            return child
                .wait_with_output()
                .expect("follow process output should be collected");
        }
        if Instant::now() >= deadline {
            child.kill().expect("hung follow process should be killed");
            let output = child
                .wait_with_output()
                .expect("killed process output should be collected");
            panic!(
                "logs --follow did not terminate: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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
fn logs_follow_waits_past_uncommitted_run_finished_then_accepts_composite_commit() {
    let sandbox = sandbox_with_journal();
    seed_matching_result_and_trace(&sandbox);
    let store = store(&sandbox);
    let runs = sandbox.home().join(".litci/runs");
    let guard = store
        .acquire_run_publication_guard(&runs, RUN_ID)
        .expect("active writer should hold publication lock");
    store
        .record_run_state(RUN_ID, None, "resolved")
        .expect("resolved state should persist");

    let mut child = spawn_follow(&sandbox);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child
            .try_wait()
            .expect("follow process should remain observable")
            .is_none(),
        "RunFinished plus physical result/trace must not stop follow before catalog commit"
    );

    store
        .record_run_state(RUN_ID, None, "completed")
        .expect("composite commit should become durable");
    drop(guard);
    let output = wait_for_output(child);
    assert!(output.status.success(), "{}", stderr_text(&output));
}

#[test]
fn logs_follow_reports_catalog_abort_instead_of_accepting_run_finished() {
    let sandbox = sandbox_with_journal();
    let store = store(&sandbox);
    let runs = sandbox.home().join(".litci/runs");
    let guard = store
        .acquire_run_publication_guard(&runs, RUN_ID)
        .expect("active writer should hold publication lock");
    store
        .record_run_state(RUN_ID, None, "resolved")
        .expect("resolved state should persist");

    let mut child = spawn_follow(&sandbox);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child
            .try_wait()
            .expect("follow process should remain observable")
            .is_none(),
        "an uncommitted RunFinished event must not terminate follow"
    );

    drop(guard);
    let output = wait_for_output(child);
    assert!(!output.status.success());
    assert!(
        stderr_text(&output).contains("aborted before a composite terminal commit"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(
        store
            .run_state(RUN_ID)
            .expect("recovered state should read"),
        Some(RunCatalogState::Aborted)
    );
}

#[test]
fn logs_rejects_runs_without_an_event_journal() {
    let sandbox = Sandbox::new();
    let directory = private_run_directory(&sandbox);
    write_private(&directory.join("result.json"), r#"{"schema_version":1}"#);
    let output = sandbox.run(&["logs", RUN_ID]);
    assert!(!output.status.success());
    assert!(stderr_text(&output).contains("has no structured log journal"));
}
