use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use greenlit_store::cas::{CasError, CasStore, EnsureOutcome, HttpFetch, ObjectDigest};
use greenlit_store::oci::RegistryResolver;
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

#[test]
fn interrupted_partial_content_is_resumed_at_its_verified_offset() {
    let temp = TempDir::new().expect("temp root should be created");
    let store = CasStore::open(temp.path()).expect("store should open");
    let expected = digest(b"resume-me");
    let hex = expected
        .as_str()
        .strip_prefix("sha256:")
        .expect("validated digest should have prefix");
    let partial = temp.path().join("tmp").join(format!("{hex}.partial"));
    fs::write(&partial, b"resume").expect("partial should be seeded");
    fs::write(
        temp.path().join("inflight").join(hex),
        "4294967295 stale-boot 0\n",
    )
    .expect("dead process lock should be seeded");
    let outcome = store
        .ensure_with(&expected, |path, offset| {
            assert_eq!(path, partial);
            assert_eq!(offset, 6);
            let mut output = OpenOptions::new()
                .append(true)
                .open(path)
                .map_err(|source| CasError::Io {
                    path: path.display().to_string(),
                    source,
                })?;
            output.write_all(b"-me").map_err(|source| CasError::Io {
                path: path.display().to_string(),
                source,
            })
        })
        .expect("resumed ensure should succeed");
    assert_eq!(outcome, EnsureOutcome::Published);
    assert_eq!(
        store.read_verified(&expected).expect("read should work"),
        Some(b"resume-me".to_vec())
    );
}

#[test]
fn leases_block_gc_and_inconsistent_metadata_blocks_destruction() {
    let temp = TempDir::new().expect("temp root should be created");
    let store = CasStore::open(temp.path()).expect("store should open");
    let leased = digest(b"leased");
    let reclaimable = digest(b"reclaimable");
    store
        .put_verified(&leased, b"leased")
        .expect("leased object should publish");
    store
        .put_verified(&reclaimable, b"reclaimable")
        .expect("reclaimable object should publish");
    store
        .acquire_lease(
            "active-run",
            std::slice::from_ref(&leased),
            std::time::Duration::from_secs(60),
        )
        .expect("active lease should persist");
    store
        .record_run_state("active-run", None, "completed")
        .expect("terminal transition should persist");
    store
        .record_run_state("reclaimable-run", None, "aborted")
        .expect("aborted transition should persist");
    assert_eq!(
        store
            .reclaimable_run_ids()
            .expect("recovery identities should load"),
        vec!["reclaimable-run".to_string()],
        "a terminal run with an active lease is not safe to reconcile"
    );
    fs::write(temp.path().join("tmp/interrupted.partial"), b"partial")
        .expect("partial download should be retained");

    let preview = store.doctor().expect("store should be consistent");
    assert!(preview.is_consistent());
    assert_eq!(preview.active_leases, 1);
    assert_eq!(preview.reclaimable_objects, 1);
    assert_eq!(preview.partial_downloads, 1);
    let collected = store
        .collect_garbage()
        .expect("safe collection should succeed");
    assert_eq!(collected.partial_downloads, 1);
    assert_eq!(collected.objects, 1);
    assert_eq!(
        store.read_verified(&leased).expect("leased read"),
        Some(b"leased".to_vec())
    );

    store
        .release_lease("active-run")
        .expect("lease should release");
    fs::remove_file(object_path(temp.path(), &leased))
        .expect("test should create inconsistent metadata");
    let inconsistent = store.doctor().expect("doctor should report inconsistency");
    assert!(!inconsistent.is_consistent());
    assert!(matches!(
        store.collect_garbage(),
        Err(CasError::CatalogState { .. })
    ));
}

#[test]
fn interrupted_http_download_resumes_by_range_and_offline_requires_the_exact_object() {
    let temp = TempDir::new().expect("temp root should be created");
    let store = CasStore::open(temp.path()).expect("store should open");
    let expected = digest(b"resume-me");
    let hex = expected
        .as_str()
        .strip_prefix("sha256:")
        .expect("validated digest should have prefix");
    fs::write(
        temp.path().join("tmp").join(format!("{hex}.partial")),
        b"resume",
    )
    .expect("partial should be seeded");

    let listener = TcpListener::bind("127.0.0.1:0").expect("server should bind");
    let address = listener
        .local_addr()
        .expect("server address should resolve");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("request should arrive");
        let mut request = [0_u8; 2048];
        let read = stream.read(&mut request).expect("request should read");
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("range: bytes=6-")),
            "resume request must name the retained offset:\n{request}"
        );
        stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 6-8/9\r\nConnection: close\r\n\r\n-me",
            )
            .expect("response should write");
    });
    let fetch = HttpFetch {
        url: format!("http://{address}/bundle"),
        user_agent: "greenlit-test".to_string(),
        max_bytes: 64,
        offline: false,
    };
    assert_eq!(
        store
            .ensure_http(&expected, &fetch)
            .expect("range download should resume"),
        EnsureOutcome::Published
    );
    server.join().expect("server should finish");

    let offline_hit = HttpFetch {
        offline: true,
        ..fetch.clone()
    };
    assert_eq!(
        store
            .ensure_http(&expected, &offline_hit)
            .expect("verified content should work offline"),
        EnsureOutcome::Hit
    );

    let missing = digest(b"not-cached");
    assert!(matches!(
        store.ensure_http(&missing, &offline_hit),
        Err(CasError::OfflineMissing { digest, source_url })
            if digest == missing && source_url == fetch.url
    ));
}

#[test]
fn canonical_tree_round_trip_preserves_files_links_modes_and_alias_identity() {
    let temp = TempDir::new().expect("temp root should be created");
    let source = temp.path().join("source");
    let restored = temp.path().join("restored");
    fs::create_dir_all(source.join("nested")).expect("source tree should be created");
    fs::write(source.join("nested/action.js"), b"console.log('ok')\n")
        .expect("source file should be written");
    fs::set_permissions(
        source.join("nested/action.js"),
        fs::Permissions::from_mode(0o751),
    )
    .expect("source mode should be set");
    std::os::unix::fs::symlink("nested/action.js", source.join("main"))
        .expect("source link should be created");

    let store = CasStore::open(temp.path().join("cas")).expect("store should open");
    let tree = store.put_tree(&source).expect("tree should ingest");
    store
        .record_alias("action-commit", "owner/repo@commit", &tree)
        .expect("alias should record");
    assert_eq!(
        store
            .resolve_alias("action-commit", "owner/repo@commit")
            .expect("alias should resolve"),
        Some(tree.clone())
    );

    fs::create_dir(&restored).expect("restore root should be created");
    store
        .materialize_tree(&tree, &restored)
        .expect("tree should materialize");
    assert_eq!(
        fs::read(restored.join("nested/action.js")).expect("file should read"),
        b"console.log('ok')\n"
    );
    assert_eq!(
        fs::symlink_metadata(restored.join("nested/action.js"))
            .expect("metadata should read")
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    assert_eq!(
        fs::read_link(restored.join("main")).expect("link should read"),
        std::path::PathBuf::from("nested/action.js")
    );
}

#[test]
fn registry_index_resolution_selects_and_verifies_the_linux_amd64_manifest() {
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let config_digest = digest(config);
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","size":{},"digest":"{}"}},"layers":[]}}"#,
        config.len(),
        config_digest
    );
    let manifest_digest = digest(manifest.as_bytes());
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","size":{},"digest":"{}","platform":{{"architecture":"amd64","os":"linux"}}}}]}}"#,
        manifest.len(),
        manifest_digest
    );
    let index_digest = digest(index.as_bytes());

    let listener = TcpListener::bind("127.0.0.1:0").expect("registry should bind");
    let address = listener
        .local_addr()
        .expect("registry address should resolve");
    let manifest_for_server = manifest.clone();
    let index_for_server = index.clone();
    let config_for_server = config.to_vec();
    let manifest_digest_for_server = manifest_digest.clone();
    let config_digest_for_server = config_digest.clone();
    let index_digest_for_server = index_digest.clone();
    let server = std::thread::spawn(move || {
        for request_number in 0..6 {
            let (mut stream, _) = listener.accept().expect("registry request should arrive");
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).expect("request should read");
            let request = String::from_utf8_lossy(&request[..read]);
            let first = request.lines().next().unwrap_or_default();
            let (status, headers, body): (&str, String, &[u8]) = match request_number {
                0 => (
                    "401 Unauthorized",
                    format!(
                        "WWW-Authenticate: Bearer realm=\"http://{address}/token\",service=\"local\",scope=\"repository:repo:pull\"\r\n"
                    ),
                    b"",
                ),
                1 => (
                    "200 OK",
                    "Content-Type: application/json\r\n".to_string(),
                    br#"{"token":"registry-token"}"#,
                ),
                2 => {
                    assert!(first.starts_with("HEAD /v2/repo/manifests/test "));
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("authorization: bearer registry-token")
                    );
                    (
                        "200 OK",
                        format!("Docker-Content-Digest: {index_digest_for_server}\r\n"),
                        b"",
                    )
                }
                3 => {
                    assert!(first.starts_with("GET /v2/repo/manifests/test "));
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("authorization: bearer registry-token")
                    );
                    (
                        "200 OK",
                        "Content-Type: application/vnd.oci.image.index.v1+json\r\n".to_string(),
                        index_for_server.as_bytes(),
                    )
                }
                4 => {
                    assert!(first.contains(&format!(
                        "/v2/repo/manifests/{}",
                        manifest_digest_for_server
                    )));
                    (
                        "200 OK",
                        "Content-Type: application/vnd.oci.image.manifest.v1+json\r\n".to_string(),
                        manifest_for_server.as_bytes(),
                    )
                }
                _ => {
                    assert!(first.contains(&format!("/v2/repo/blobs/{config_digest_for_server}")));
                    (
                        "200 OK",
                        "Content-Type: application/octet-stream\r\n".to_string(),
                        config_for_server.as_slice(),
                    )
                }
            };
            let response = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .and_then(|()| stream.write_all(body))
                .expect("registry response should write");
        }
    });

    let temp = TempDir::new().expect("temp root should be created");
    let store = CasStore::open(temp.path()).expect("store should open");
    let resolver = RegistryResolver::new(store.clone());
    let resolved = resolver
        .resolve_linux_amd64(&format!("{address}/repo:test"))
        .expect("platform should resolve");
    assert_eq!(resolved.digest, manifest_digest);
    assert_eq!(
        resolved.pull_reference,
        format!("{address}/repo@{}", resolved.digest)
    );
    assert_eq!(resolved.os, "linux");
    assert_eq!(resolved.architecture, "amd64");
    assert_eq!(
        store
            .read_verified(&resolved.digest)
            .expect("manifest should verify"),
        Some(manifest.into_bytes())
    );
    assert_eq!(
        store
            .read_verified(&config_digest)
            .expect("config should verify"),
        Some(config.to_vec())
    );
    server.join().expect("registry should finish");
}
