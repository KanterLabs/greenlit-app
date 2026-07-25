//! The `actions/cache` store's behavior contract: what a save makes visible,
//! what an interrupted save does not, and how a duplicate key is answered.
//!
//! The selection *rule* is pinned by the oracle table in
//! `greenlit_store::cache::key`; these tests cover the durable behavior
//! around it that only a real filesystem exercises.

use greenlit_store::{CacheStore, StoreError};

/// Saves `key` with `body` in one chunk and returns its committed id.
fn save(store: &CacheStore, key: &str, version: &str, body: &[u8]) -> i64 {
    let id = store.reserve(key, version).expect("reserve");
    store.write_chunk(id, 0, body).expect("upload");
    let size = u64::try_from(body.len()).expect("body length fits u64");
    store.commit(id, size).expect("commit");
    id
}

#[test]
fn a_saved_entry_restores_by_its_exact_key() {
    let root = tempfile::tempdir().expect("temp root");
    let store = CacheStore::at(root.path());

    let id = save(&store, "npm-abc", "v1", b"payload");

    let restored = store
        .lookup(&["npm-abc".to_string()], "v1")
        .expect("lookup")
        .expect("hit");
    assert_eq!(restored.id, id);
    assert_eq!(restored.key, "npm-abc");
    assert!(restored.exact);

    let blob = store.blob_path(id).expect("blob path");
    assert_eq!(std::fs::read(blob).expect("read blob"), b"payload");
}

#[test]
fn a_restore_key_matches_by_prefix_and_reports_a_partial_hit() {
    let root = tempfile::tempdir().expect("temp root");
    let store = CacheStore::at(root.path());

    save(&store, "npm-abc123", "v1", b"payload");

    let restored = store
        .lookup(&["npm-zzz".to_string(), "npm-".to_string()], "v1")
        .expect("lookup")
        .expect("hit");
    assert_eq!(restored.key, "npm-abc123");
    assert!(
        !restored.exact,
        "a restore-key match is partial, which is what drives cache-hit: false"
    );
}

#[test]
fn an_entry_saved_at_another_version_is_never_restored() {
    let root = tempfile::tempdir().expect("temp root");
    let store = CacheStore::at(root.path());

    save(&store, "npm-abc", "paths-a", b"payload");

    assert!(
        store
            .lookup(&["npm-abc".to_string()], "paths-b")
            .expect("lookup")
            .is_none(),
        "the version scopes the whole lookup, so a different path set misses"
    );
}

#[test]
fn an_uncommitted_upload_is_invisible_to_lookup() {
    let root = tempfile::tempdir().expect("temp root");
    let store = CacheStore::at(root.path());

    // Reserve and upload, but never commit -- the shape a killed run leaves.
    let id = store.reserve("npm-abc", "v1").expect("reserve");
    store.write_chunk(id, 0, b"half").expect("upload");

    assert!(
        store
            .lookup(&["npm-abc".to_string()], "v1")
            .expect("lookup")
            .is_none(),
        "an interrupted save must never be restorable as if it were complete"
    );
    assert!(
        store.blob_path(id).is_err(),
        "and its blob is not addressable either"
    );
}

#[test]
fn reserving_an_already_committed_key_is_refused() {
    let root = tempfile::tempdir().expect("temp root");
    let store = CacheStore::at(root.path());

    save(&store, "npm-abc", "v1", b"payload");

    // The hosted service answers this with HTTP 409, which `actions/cache`
    // treats as "another job saved it first" rather than a failure.
    let again = store.reserve("npm-abc", "v1");
    assert!(
        matches!(again, Err(StoreError::AlreadyReserved { ref key }) if key == "npm-abc"),
        "expected AlreadyReserved, got {again:?}"
    );

    // The same key at a different version is a different entry entirely.
    store
        .reserve("npm-abc", "v2")
        .expect("a different version reserves cleanly");
}

#[test]
fn chunks_land_at_their_content_range_offsets() {
    let root = tempfile::tempdir().expect("temp root");
    let store = CacheStore::at(root.path());

    // `actions/cache` uploads concurrently, so a later range can arrive
    // first; the store addresses writes by offset rather than appending.
    let id = store.reserve("npm-abc", "v1").expect("reserve");
    store.write_chunk(id, 5, b"world").expect("second chunk");
    store.write_chunk(id, 0, b"hello").expect("first chunk");
    store.commit(id, 10).expect("commit");

    let blob = store.blob_path(id).expect("blob path");
    assert_eq!(std::fs::read(blob).expect("read blob"), b"helloworld");
}

#[test]
fn uploading_against_an_unknown_reservation_is_refused() {
    let root = tempfile::tempdir().expect("temp root");
    let store = CacheStore::at(root.path());

    assert!(matches!(
        store.write_chunk(4242, 0, b"x"),
        Err(StoreError::UnknownReservation { id: 4242 })
    ));
    assert!(matches!(
        store.commit(4242, 1),
        Err(StoreError::UnknownReservation { id: 4242 })
    ));
}

#[test]
fn ids_are_not_reused_across_saves() {
    let root = tempfile::tempdir().expect("temp root");
    let store = CacheStore::at(root.path());

    let first = save(&store, "a", "v1", b"1");
    let second = save(&store, "b", "v1", b"2");
    let third = store.reserve("c", "v1").expect("reserve");

    assert_ne!(first, second);
    assert_ne!(second, third);
    assert_ne!(first, third);
}

#[test]
fn an_empty_store_misses_rather_than_failing() {
    let root = tempfile::tempdir().expect("temp root");
    let store = CacheStore::at(root.path().join("never-created"));

    assert!(
        store
            .lookup(&["anything".to_string()], "v1")
            .expect("a store with no directory yet is empty, not broken")
            .is_none()
    );
}

#[test]
fn the_default_root_is_under_the_home_directory() {
    assert_eq!(
        CacheStore::default_path_under(std::path::Path::new("/home/user")),
        std::path::Path::new("/home/user/.litci/cache")
    );
}
