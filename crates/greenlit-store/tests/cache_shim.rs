//! The shim's wire contract, driven over real HTTP.
//!
//! `actions/cache` is unmodified code that Greenlit does not control, so what
//! matters is not that the store works but that *this exact request sequence*
//! behaves the way the hosted service behaves. These tests reproduce the
//! sequence `@actions/cache` makes — the URLs it builds, the bodies it sends,
//! and the status codes it branches on — against a real listener.
//!
//! Sequence, from `packages/cache/src/internal/cacheHttpClient.ts`:
//! `getCacheEntry` (204 → miss, 200 → restore), then on a miss
//! `reserveCache` → `uploadChunk`×n → `commitCache`, and on a hit a plain GET
//! of the returned `archiveLocation`.

use std::net::Ipv4Addr;

use greenlit_store::{CacheStore, ShimState};

const TOKEN: &str = "runtime-token-for-this-run";
/// Blob URLs authorize themselves; no client sends a header for them.
const SIGNATURE: &str = "url-signature-for-this-run";

/// A started shim plus the base URL a client would be handed.
struct Fixture {
    _root: tempfile::TempDir,
    shim: greenlit_store::Shim,
    base: String,
}

async fn start() -> Fixture {
    let root = tempfile::tempdir().expect("temp root");

    // Bind first so the port is known, then build the state around it: the
    // `archiveLocation` the shim hands back has to be fetchable, and on a
    // real run the same port goes into the job's `ACTIONS_CACHE_URL`. The
    // real run points the base at the bridge gateway; a test binds loopback,
    // which exercises the identical code path.
    let bound = greenlit_store::bind(Ipv4Addr::LOCALHOST)
        .await
        .expect("bind the shim");
    let base = format!("http://127.0.0.1:{}/", bound.address().port());
    let state = ShimState::new(
        CacheStore::at(root.path()),
        greenlit_store::ArtifactStore::at(root.path().join("artifacts")),
        TOKEN,
        SIGNATURE,
        base.clone(),
    );
    let shim = bound.serve(state);

    Fixture {
        _root: root,
        shim,
        base,
    }
}

/// `getCacheEntry`'s URL: `${base}_apis/artifactcache/cache?keys=…&version=…`.
fn lookup_url(base: &str, keys: &[&str], version: &str) -> String {
    let joined = keys.join(",");
    let encoded = joined.replace(',', "%2C");
    format!("{base}_apis/artifactcache/cache?keys={encoded}&version={version}")
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_defaults()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_full_save_then_restore_sequence_matches_the_client() {
    let fixture = start().await;
    let base = fixture.base.clone();
    let agent = agent();

    // 1. getCacheEntry on an empty store: 204, which the client reads as null.
    let miss = agent
        .get(&lookup_url(&base, &["npm-abc", "npm-"], "v-paths"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .expect("lookup");
    assert_eq!(miss.status(), 204, "a miss is 204 No Content");

    // 2. reserveCache -> {cacheId}
    let mut reserved = agent
        .post(&format!("{base}_apis/artifactcache/caches"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send_json(serde_json::json!({
            "key": "npm-abc",
            "version": "v-paths",
            "cacheSize": 11,
        }))
        .expect("reserve");
    assert_eq!(reserved.status(), 201);
    let body: serde_json::Value = reserved.body_mut().read_json().expect("reserve body");
    let cache_id = body
        .get("cacheId")
        .and_then(serde_json::Value::as_i64)
        .expect("the client reads cacheId out of this exact field");

    // 3. uploadChunk, twice, at explicit Content-Range offsets.
    for (offset, chunk) in [(0_usize, "hello "), (6, "cache")] {
        let end = offset + chunk.len() - 1;
        let response = agent
            .patch(&format!("{base}_apis/artifactcache/caches/{cache_id}"))
            .header("Authorization", &format!("Bearer {TOKEN}"))
            .header("Content-Type", "application/octet-stream")
            .header("Content-Range", &format!("bytes {offset}-{end}/*"))
            .send(chunk)
            .expect("upload chunk");
        assert_eq!(response.status(), 200);
    }

    // 4. commitCache
    let committed = agent
        .post(&format!("{base}_apis/artifactcache/caches/{cache_id}"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send_json(serde_json::json!({ "size": 11 }))
        .expect("commit");
    assert_eq!(committed.status(), 200);

    // 5. getCacheEntry again: now a hit carrying the matched key and a URL.
    let mut hit = agent
        .get(&lookup_url(&base, &["npm-abc", "npm-"], "v-paths"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .expect("lookup after save");
    assert_eq!(hit.status(), 200);
    let entry: serde_json::Value = hit.body_mut().read_json().expect("hit body");
    assert_eq!(
        entry.get("cacheKey").and_then(serde_json::Value::as_str),
        Some("npm-abc"),
        "the client compares cacheKey against its primary key to set cache-hit"
    );
    let location = entry
        .get("archiveLocation")
        .and_then(serde_json::Value::as_str)
        .expect("archiveLocation drives the download");

    // 6. Download whatever archiveLocation pointed at.
    let mut archive = agent
        .get(location)
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .expect("download");
    assert_eq!(archive.status(), 200);
    assert_eq!(
        archive.body_mut().read_to_string().expect("archive body"),
        "hello cache",
        "the bytes that come back are the bytes that went in"
    );

    fixture.shim.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_key_hit_reports_the_key_that_actually_matched() {
    let fixture = start().await;
    let base = fixture.base.clone();
    let agent = agent();
    let auth = format!("Bearer {TOKEN}");

    let mut reserved = agent
        .post(&format!("{base}_apis/artifactcache/caches"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({ "key": "npm-lock-999", "version": "v1" }))
        .expect("reserve");
    let id = reserved
        .body_mut()
        .read_json::<serde_json::Value>()
        .expect("body")
        .get("cacheId")
        .and_then(serde_json::Value::as_i64)
        .expect("cacheId");
    agent
        .patch(&format!("{base}_apis/artifactcache/caches/{id}"))
        .header("Authorization", &auth)
        .header("Content-Range", "bytes 0-2/*")
        .send("abc")
        .expect("upload");
    agent
        .post(&format!("{base}_apis/artifactcache/caches/{id}"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({ "size": 3 }))
        .expect("commit");

    // Primary key misses; the restore key matches by prefix.
    let mut hit = agent
        .get(&lookup_url(&base, &["npm-lock-000", "npm-lock-"], "v1"))
        .header("Authorization", &auth)
        .call()
        .expect("lookup");
    assert_eq!(hit.status(), 200);
    let entry: serde_json::Value = hit.body_mut().read_json().expect("body");
    assert_eq!(
        entry.get("cacheKey").and_then(serde_json::Value::as_str),
        Some("npm-lock-999"),
        "a partial hit must report the stored key, not the requested one, or \
         the action would report cache-hit: true for a restore-key match"
    );

    fixture.shim.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_key_is_answered_with_conflict() {
    let fixture = start().await;
    let base = fixture.base.clone();
    let agent = agent();
    let auth = format!("Bearer {TOKEN}");

    let mut reserved = agent
        .post(&format!("{base}_apis/artifactcache/caches"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({ "key": "dup", "version": "v1" }))
        .expect("reserve");
    let id = reserved
        .body_mut()
        .read_json::<serde_json::Value>()
        .expect("body")
        .get("cacheId")
        .and_then(serde_json::Value::as_i64)
        .expect("cacheId");
    agent
        .patch(&format!("{base}_apis/artifactcache/caches/{id}"))
        .header("Authorization", &auth)
        .header("Content-Range", "bytes 0-0/*")
        .send("x")
        .expect("upload");
    agent
        .post(&format!("{base}_apis/artifactcache/caches/{id}"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({ "size": 1 }))
        .expect("commit");

    // `actions/cache` treats 409 as "another job saved it first" and moves on.
    let again = agent
        .post(&format!("{base}_apis/artifactcache/caches"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({ "key": "dup", "version": "v1" }));
    let status = match again {
        Ok(response) => response.status(),
        Err(ureq::Error::StatusCode(code)) => {
            ureq::http::StatusCode::from_u16(code).expect("status")
        }
        Err(other) => panic!("expected a status error, got {other:?}"),
    };
    assert_eq!(status, 409);

    fixture.shim.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_route_refuses_a_request_without_the_runtime_token() {
    let fixture = start().await;
    let base = fixture.base.clone();
    let agent = agent();

    // The shim sits on the job network alongside service containers and
    // Docker actions, so an unauthenticated route would expose one run's
    // cache to anything else on that bridge.
    let unauthenticated = [
        lookup_url(&base, &["k"], "v"),
        format!("{base}_apis/artifactcache/blobs/1"),
    ];
    for url in unauthenticated {
        let response = agent.get(&url).call();
        let status = match response {
            Ok(response) => response.status().as_u16(),
            Err(ureq::Error::StatusCode(code)) => code,
            Err(other) => panic!("expected a status error, got {other:?}"),
        };
        assert_eq!(status, 401, "unauthenticated GET {url} must be refused");
    }

    let posted = agent
        .post(&format!("{base}_apis/artifactcache/caches"))
        .send_json(serde_json::json!({ "key": "k", "version": "v" }));
    let status = match posted {
        Ok(response) => response.status().as_u16(),
        Err(ureq::Error::StatusCode(code)) => code,
        Err(other) => panic!("expected a status error, got {other:?}"),
    };
    assert_eq!(status, 401, "unauthenticated reserve must be refused");

    fixture.shim.shutdown().await;
}
