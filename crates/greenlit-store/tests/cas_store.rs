use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use greenlit_store::cas::{CasError, CasStore, EnsureOutcome, ObjectDigest};
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
    let store = CasStore::open(temp.path()).expect("store should open");
    let expected = digest(b"verified");
    assert_eq!(
        store
            .put_verified(&expected, b"verified")
            .expect("object should publish"),
        EnsureOutcome::Published
    );
    fs::write(object_path(temp.path(), &expected), b"corrupt")
        .expect("test should corrupt the object");
    assert!(matches!(
        store.read_verified(&expected),
        Err(CasError::DigestMismatch { .. })
    ));
    assert!(
        fs::read_dir(temp.path().join("quarantine"))
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
}

#[test]
fn concurrent_requests_materialize_one_digest_once() {
    let temp = TempDir::new().expect("temp root should be created");
    let store = Arc::new(CasStore::open(temp.path()).expect("store should open"));
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
