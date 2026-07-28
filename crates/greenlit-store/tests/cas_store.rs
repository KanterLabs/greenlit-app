use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use greenlit_store::cas::{CasError, CasStore, EnsureOutcome, ObjectDigest, RunCatalogState};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn digest(bytes: &[u8]) -> ObjectDigest {
    let mut hex = String::new();
    for byte in Sha256::digest(bytes) {
        hex.push_str(&format!("{byte:02x}"));
    }
    ObjectDigest::parse(&format!("sha256:{hex}")).expect("test digest should be valid")
}

fn object_path(root: &std::path::Path, digest: &ObjectDigest) -> std::path::PathBuf {
    let hex = digest
        .as_str()
        .strip_prefix("sha256:")
        .expect("validated digest should have prefix");
    root.join("objects")
        .join("sha256")
        .join(&hex[..2])
        .join(&hex[2..])
}

#[test]
fn corrupted_content_is_quarantined_and_can_be_refetched() {
    let temp = TempDir::new().expect("temp root should be created");
    let root = temp.path().join("cas");
    let store = CasStore::open(&root).expect("store should open");
    let expected = digest(b"verified");
    assert_eq!(
        store
            .put_verified(&expected, b"verified")
            .expect("object should publish"),
        EnsureOutcome::Published
    );
    fs::write(object_path(&root, &expected), b"corrupt").expect("test should corrupt the object");
    assert!(matches!(
        store.read_verified(&expected),
        Err(CasError::DigestMismatch { .. })
    ));
    assert!(
        fs::read_dir(root.join("quarantine"))
            .expect("quarantine should exist")
            .next()
            .is_some()
    );
    assert_eq!(
        store
            .ensure_with(&expected, |partial, offset| {
                assert_eq!(offset, 0);
                fs::write(partial, b"verified").map_err(|source| CasError::Io {
                    path: partial.display().to_string(),
                    source,
                })
            })
            .expect("object should refetch"),
        EnsureOutcome::Published
    );
    assert_eq!(
        store.read_verified(&expected).expect("read should work"),
        Some(b"verified".to_vec())
    );

    let linked_home = TempDir::new().expect("symlinked state HOME");
    let redirected = TempDir::new().expect("redirected state target");
    std::os::unix::fs::symlink(redirected.path(), linked_home.path().join(".litci"))
        .expect("create symlinked .litci ancestor");
    let linked_store = CasStore::default_path_under(linked_home.path());
    assert!(
        CasStore::open(&linked_store).is_err(),
        "CAS accepted a symlinked .litci ancestor"
    );
    assert!(
        !redirected.path().join("store").exists(),
        "CAS created content through a symlinked ancestor"
    );
}

#[test]
fn concurrent_requests_materialize_one_digest_once() {
    let temp = TempDir::new().expect("temp root should be created");
    let store = Arc::new(CasStore::open(temp.path().join("cas")).expect("store should open"));
    let expected = digest(b"shared");
    let starts = Arc::new(Barrier::new(8));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        let expected = expected.clone();
        let starts = starts.clone();
        let calls = calls.clone();
        threads.push(std::thread::spawn(move || {
            starts.wait();
            store.ensure_with(&expected, |partial, _offset| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(50));
                fs::write(partial, b"shared").map_err(|source| CasError::Io {
                    path: partial.display().to_string(),
                    source,
                })
            })
        }));
    }
    for thread in threads {
        thread
            .join()
            .expect("request thread should finish")
            .expect("ensure should succeed");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn recovery_revokes_every_incomplete_publication_window_before_quarantine() {
    let temp = TempDir::new().expect("temp root should be created");
    let store = CasStore::open(temp.path().join("cas")).expect("store should open");
    let runs = temp.path().join("runs");
    fs::create_dir(&runs).expect("runs root should be created");
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o700))
        .expect("runs root should be private");

    let windows = [
        (
            "aaa1",
            vec![(
                "events.ndjson",
                "{\"type\":\"run_finished\",\"conclusion\":\"Passed\"}\n",
            )],
        ),
        (
            "aaa2",
            vec![
                (
                    "events.ndjson",
                    "{\"type\":\"run_finished\",\"conclusion\":\"Passed\"}\n",
                ),
                (
                    "trace.ndjson",
                    "{\"event\":\"run_completed\",\"attributes\":{}}\n",
                ),
            ],
        ),
        (
            "aaa3",
            vec![
                (
                    "events.ndjson",
                    "{\"type\":\"run_finished\",\"conclusion\":\"Passed\"}\n",
                ),
                (
                    "trace.ndjson",
                    "{\"event\":\"run_completed\",\"attributes\":{}}\n",
                ),
                (
                    "result.json",
                    "{\"schema_version\":1,\"conclusion\":\"passed\"}\n",
                ),
            ],
        ),
    ];
    for (run_id, artifacts) in &windows {
        let directory = runs.join(run_id);
        fs::create_dir(&directory).expect("run directory should be created");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("run directory should be private");
        for (name, contents) in artifacts {
            let path = directory.join(name);
            fs::write(&path, contents).expect("publication artifact should be written");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .expect("publication artifact should be private");
        }
        drop(
            store
                .acquire_run_publication_guard(&runs, run_id)
                .expect("publication liveness lock should be created"),
        );
        store
            .record_run_state(run_id, None, "resolved")
            .expect("incomplete catalog state should persist");
    }

    let completed_id = "aaa4";
    let completed = runs.join(completed_id);
    fs::create_dir(&completed).expect("completed run directory should be created");
    fs::set_permissions(&completed, fs::Permissions::from_mode(0o700))
        .expect("completed run directory should be private");
    drop(
        store
            .acquire_run_publication_guard(&runs, completed_id)
            .expect("completed run liveness lock should be created"),
    );
    store
        .record_run_state(completed_id, None, "completed")
        .expect("completed catalog state should persist");

    let active_id = "aaa5";
    let active = runs.join(active_id);
    fs::create_dir(&active).expect("active run directory should be created");
    fs::set_permissions(&active, fs::Permissions::from_mode(0o700))
        .expect("active run directory should be private");
    let active_guard = store
        .acquire_run_publication_guard(&runs, active_id)
        .expect("active publisher should hold its liveness lock");
    store
        .record_run_state(active_id, None, "resolved")
        .expect("active catalog state should persist");

    let unprotected_id = "aaa6";
    let unprotected = runs.join(unprotected_id);
    fs::create_dir(&unprotected).expect("unprotected run directory should be created");
    fs::set_permissions(&unprotected, fs::Permissions::from_mode(0o700))
        .expect("unprotected run directory should be private");
    store
        .record_run_state(unprotected_id, None, "resolved")
        .expect("unprotected catalog state should persist");

    let report = store
        .recover_incomplete_run_publications(&runs)
        .expect("recovery should complete");
    assert_eq!(report.recovered.len(), windows.len());
    assert_eq!(report.active, vec![active_id]);
    assert_eq!(report.unprotected, vec![unprotected_id]);
    for (run_id, _) in &windows {
        assert_eq!(
            store.run_state(run_id).expect("state should read"),
            Some(RunCatalogState::Aborted),
            "{run_id} must be non-authoritative before cleanup"
        );
        assert!(!runs.join(run_id).exists());
        assert!(
            runs.join(".recovery-quarantine").join(run_id).exists(),
            "{run_id} should be retained outside the authoritative run set"
        );
    }
    assert_eq!(
        store
            .run_state(completed_id)
            .expect("completed state should read"),
        Some(RunCatalogState::Completed)
    );
    assert!(
        completed.exists(),
        "a completed catalog row is never recovery-cleaned"
    );
    assert_eq!(
        store
            .run_state(active_id)
            .expect("active state should read"),
        Some(RunCatalogState::Resolved)
    );
    assert!(active.exists(), "an actively locked run must not be moved");
    assert_eq!(
        store
            .run_state(unprotected_id)
            .expect("unprotected state should read"),
        Some(RunCatalogState::Resolved)
    );
    assert!(
        unprotected.exists(),
        "a missing liveness lock must prevent automatic mutation"
    );
    drop(active_guard);
    let second = store
        .recover_incomplete_run_publications(&runs)
        .expect("released active run should recover");
    assert_eq!(second.recovered.len(), 1);
    assert_eq!(
        store
            .run_state(active_id)
            .expect("recovered state should read"),
        Some(RunCatalogState::Aborted)
    );
}

#[test]
fn recovery_never_follows_quarantine_links_or_resurrects_aborted_runs() {
    let temp = TempDir::new().expect("temp root should be created");
    let store = CasStore::open(temp.path().join("cas")).expect("store should open");
    let runs = temp.path().join("runs");
    fs::create_dir(&runs).expect("runs root should be created");
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o700))
        .expect("runs root should be private");
    let run_id = "bbb1";
    let directory = runs.join(run_id);
    fs::create_dir(&directory).expect("run directory should be created");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .expect("run directory should be private");
    let result = directory.join("result.json");
    fs::write(&result, b"completed-looking").expect("orphan result should be written");
    fs::set_permissions(&result, fs::Permissions::from_mode(0o600))
        .expect("orphan result should be private");
    drop(
        store
            .acquire_run_publication_guard(&runs, run_id)
            .expect("publication liveness lock should be created"),
    );
    store
        .record_run_state(run_id, None, "resolved")
        .expect("incomplete catalog state should persist");

    let redirected = temp.path().join("redirected");
    fs::create_dir(&redirected).expect("redirect target should be created");
    std::os::unix::fs::symlink(&redirected, runs.join(".recovery-quarantine"))
        .expect("unsafe quarantine link should be created");

    assert!(
        store.recover_incomplete_run_publications(&runs).is_err(),
        "recovery must reject a linked quarantine path"
    );
    assert_eq!(
        store.run_state(run_id).expect("state should read"),
        Some(RunCatalogState::Aborted),
        "catalog revocation must precede cleanup that can fail"
    );
    assert!(
        directory.exists(),
        "failed cleanup may retain bytes for diagnosis"
    );
    assert!(
        fs::read_dir(&redirected)
            .expect("redirect target should remain readable")
            .next()
            .is_none(),
        "recovery must not move data through a symlink"
    );
    assert!(matches!(
        store.record_run_state(run_id, None, "completed"),
        Err(CasError::InvalidRunStateTransition { .. })
    ));
}
