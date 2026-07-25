use crate::support::{Sandbox, stderr_text, stdout_text};

#[test]
fn inspect_renders_latest_persisted_lock_and_result() {
    let sandbox = Sandbox::new();
    let run_id = "00000000000000000000000000000001-00000001-0000";
    sandbox.write_home(
        &format!(".litci/runs/{run_id}/run-lock.json"),
        r#"{"schema_version":1,"source":{"snapshot_digest":"sha256:test"}}"#,
    );
    sandbox.write_home(
        &format!(".litci/runs/{run_id}/result.json"),
        r#"{"schema_version":1,"conclusion":"passed","compatibility":"degraded","assurance":"local","reasons":[]}"#,
    );

    let output = sandbox.run(&["inspect"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let document: serde_json::Value =
        serde_json::from_str(&stdout_text(&output)).expect("inspect output should be JSON");
    assert_eq!(document["run_id"], run_id);
    assert_eq!(document["lock"]["source"]["snapshot_digest"], "sha256:test");
    assert_eq!(document["result"]["assurance"], "local");
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
    let root = greenlit_store::cas::CasStore::default_path_under(sandbox.home());
    let store = greenlit_store::cas::CasStore::open(&root).expect("store should open");
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
