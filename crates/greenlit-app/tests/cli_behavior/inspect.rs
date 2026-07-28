use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use greenlit_store::cas::{CasStore, RunCatalogState};

use crate::support::{Sandbox, stderr_text, stdout_text};

const RUN_ID: &str = "00000000000000000000000000000001-00000001-0000";

fn private_run_directory(sandbox: &Sandbox, run_id: &str) -> PathBuf {
    let runs = sandbox.home().join(".litci/runs");
    let directory = runs.join(run_id);
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

fn seed_terminal_artifacts(sandbox: &Sandbox, run_id: &str) -> PathBuf {
    let directory = private_run_directory(sandbox, run_id);
    write_private(
        &directory.join("run-lock.json"),
        r#"{"schema_version":1,"source":{"snapshot_digest":"sha256:test"}}"#,
    );
    write_private(
        &directory.join("result.json"),
        r#"{"schema_version":1,"conclusion":"passed","compatibility":"supported","assurance":"local","reasons":[]}"#,
    );
    write_private(
        &directory.join("events.ndjson"),
        &format!(
            "{{\"schema_version\":1,\"sequence\":1,\"timestamp_unix_ms\":10,\"elapsed_ms\":1,\"run_id\":\"{run_id}\",\"type\":\"run_finished\",\"conclusion\":\"Passed\",\"compatibility\":\"Supported\",\"assurance\":\"Local\",\"evidence\":\"{run_id}\"}}\n"
        ),
    );
    write_private(
        &directory.join("trace.ndjson"),
        "{\"schema_version\":1,\"sequence\":1,\"event\":\"run_completed\",\"attributes\":{\"assurance\":\"Local\",\"compatibility\":\"Supported\",\"conclusion\":\"Passed\"}}\n",
    );
    directory
}

fn store(sandbox: &Sandbox) -> CasStore {
    CasStore::open(CasStore::default_path_under(sandbox.home())).expect("catalog should open")
}

#[test]
fn inspect_renders_only_composite_authoritative_terminal_evidence() {
    let sandbox = Sandbox::new();
    let directory = seed_terminal_artifacts(&sandbox, RUN_ID);
    let store = store(&sandbox);
    drop(
        store
            .acquire_run_publication_guard(
                directory.parent().expect("runs root should exist"),
                RUN_ID,
            )
            .expect("publication lock should be created"),
    );
    store
        .record_run_state(RUN_ID, None, "completed")
        .expect("completed catalog state should persist");

    let output = sandbox.run(&["inspect"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let document: serde_json::Value =
        serde_json::from_str(&stdout_text(&output)).expect("inspect output should be JSON");
    assert_eq!(document["run_id"], RUN_ID);
    assert_eq!(document["lock"]["source"]["snapshot_digest"], "sha256:test");
    assert_eq!(document["result"]["assurance"], "local");
    assert_eq!(document["terminal_authority"]["authoritative"], true);
    assert_eq!(document["terminal_authority"]["catalog_state"], "completed");
}

#[test]
fn inspect_rejects_completed_looking_files_for_noncompleted_catalog_states() {
    for (state, expected) in [
        ("resolved", "not durably completed"),
        ("aborted", "is aborted"),
    ] {
        let sandbox = Sandbox::new();
        let directory = seed_terminal_artifacts(&sandbox, RUN_ID);
        let store = store(&sandbox);
        drop(
            store
                .acquire_run_publication_guard(
                    directory.parent().expect("runs root should exist"),
                    RUN_ID,
                )
                .expect("publication lock should be created"),
        );
        store
            .record_run_state(RUN_ID, None, state)
            .expect("catalog state should persist");

        let output = sandbox.run(&["inspect", RUN_ID]);
        assert!(
            !output.status.success(),
            "{state} must not be authoritative"
        );
        let stderr = stderr_text(&output);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(
            stderr.contains("completion-looking files are not authoritative"),
            "{stderr}"
        );
    }
}

#[test]
fn inspect_rejects_a_completed_catalog_row_without_matching_composite_artifacts() {
    let sandbox = Sandbox::new();
    let directory = seed_terminal_artifacts(&sandbox, RUN_ID);
    fs::remove_file(directory.join("trace.ndjson")).expect("trace should be removed");
    let store = store(&sandbox);
    store
        .record_run_state(RUN_ID, None, "completed")
        .expect("completed catalog state should persist");

    let output = sandbox.run(&["inspect", RUN_ID]);
    assert!(!output.status.success());
    assert!(
        stderr_text(&output).contains("trace.ndjson"),
        "{}",
        stderr_text(&output)
    );
}

#[test]
fn inspect_rejects_path_traversal_as_a_run_identity() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["inspect", "../../etc"]);
    assert!(!output.status.success());
    assert!(stderr_text(&output).contains("invalid run identity"));
}

#[test]
fn doctor_reports_reclaimable_bytes_without_deleting_content() {
    let sandbox = Sandbox::new();
    let root = CasStore::default_path_under(sandbox.home());
    let store = CasStore::open(&root).expect("store should open");
    let digest = greenlit_store::cas::ObjectDigest::of_bytes(b"reclaimable");
    store
        .put_verified(&digest, b"reclaimable")
        .expect("object should publish");

    let output = sandbox.run(&["doctor", "--json"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let document: serde_json::Value =
        serde_json::from_str(&stdout_text(&output)).expect("doctor output should be JSON");
    assert_eq!(document["consistent"], true);
    assert_eq!(document["reclaimable_objects"], 1);
    assert_eq!(document["reclaimable_bytes"], 11);
    assert_eq!(
        store.read_verified(&digest).expect("object should remain"),
        Some(b"reclaimable".to_vec()),
        "doctor is read-only"
    );
}

#[test]
fn doctor_reports_an_inactive_orphan_without_mutating_it() {
    let sandbox = Sandbox::new();
    let directory = seed_terminal_artifacts(&sandbox, RUN_ID);
    let store = store(&sandbox);
    drop(
        store
            .acquire_run_publication_guard(
                directory.parent().expect("runs root should exist"),
                RUN_ID,
            )
            .expect("publication lock should be created"),
    );
    store
        .record_run_state(RUN_ID, None, "resolved")
        .expect("resolved catalog state should persist");

    let output = sandbox.run(&["doctor", "--json"]);
    assert!(!output.status.success());
    let document: serde_json::Value =
        serde_json::from_str(&stdout_text(&output)).expect("doctor output should be JSON");
    assert_eq!(document["consistent"], false);
    assert_eq!(document["interrupted_runs"][0], RUN_ID);
    assert!(
        directory.exists(),
        "doctor reports recovery work but remains read-only"
    );
    assert_eq!(
        store
            .run_state(RUN_ID)
            .expect("state should remain readable"),
        Some(RunCatalogState::Resolved)
    );
}
