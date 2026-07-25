//! The artifact shim's wire contract, driven over real HTTP.
//!
//! `upload-artifact@v4` is unmodified code Greenlit does not control, so what
//! matters is that *its* request sequence behaves the way the hosted service
//! behaves. These tests reproduce that sequence: a twirp `CreateArtifact`, an
//! Azure Block Blob upload to whatever URL it returned, a twirp
//! `FinalizeArtifact`, then the download side's `ListArtifacts` →
//! `GetSignedArtifactURL` → `GET`.
//!
//! Field naming is asymmetric and the tests assert it deliberately: the
//! generated client sends `useProtoFieldName: true` (snake_case) but parses
//! responses without it (lowerCamelCase). Because it also passes
//! `ignoreUnknownFields`, a snake_case *response* is silently ignored rather
//! than rejected — so a shim that got this wrong would look correct in every
//! hand-written test while leaving the real client with an empty upload URL.

use std::net::Ipv4Addr;

use greenlit_store::{ArtifactStore, CacheStore, ShimState};

const TOKEN: &str = "runtime-token-for-this-run";
/// Blob URLs authorize themselves; no client sends a header for them.
const SIGNATURE: &str = "url-signature-for-this-run";
const SERVICE: &str = "github.actions.results.api.v1.ArtifactService";
const SCOPE: &str = "run-backend-id-1";

struct Fixture {
    _root: tempfile::TempDir,
    shim: greenlit_store::Shim,
    base: String,
}

async fn start() -> Fixture {
    let root = tempfile::tempdir().expect("temp root");
    let bound = greenlit_store::bind(Ipv4Addr::LOCALHOST)
        .await
        .expect("bind the shim");
    let base = format!("http://127.0.0.1:{}/", bound.address().port());
    let state = ShimState::new(
        CacheStore::at(root.path().join("cache")),
        ArtifactStore::at(root.path().join("artifacts")),
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

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_defaults()
}

/// One twirp call, returning the decoded body.
fn twirp(
    agent: &ureq::Agent,
    base: &str,
    method: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let mut response = agent
        .post(&format!("{base}twirp/{SERVICE}/{method}"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send_json(body)
        .unwrap_or_else(|error| panic!("{method} failed: {error:?}"));
    response.body_mut().read_json().expect("twirp body")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_upload_then_download_sequence_matches_the_client() {
    let fixture = start().await;
    let base = fixture.base.clone();
    let agent = agent();

    // 1. CreateArtifact -> a URL to upload to.
    let created = twirp(
        &agent,
        &base,
        "CreateArtifact",
        serde_json::json!({
            "workflow_run_backend_id": SCOPE,
            "workflow_job_run_backend_id": "job-1",
            "name": "build-output",
            "version": 4,
        }),
    );
    assert_eq!(
        created.get("ok").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    let upload_url = created
        .get("signedUploadUrl")
        .and_then(serde_json::Value::as_str)
        .expect("the client uploads to exactly this URL")
        .to_string();

    // 2. Stage two blocks out of order, then commit an ordering. The upload
    //    is concurrent, so the commit document is the only thing that says
    //    how the blocks assemble -- honouring arrival order instead would
    //    produce a corrupt artifact that still finalizes cleanly.
    for (block_id, chunk) in [("YjAwMDI=", "world"), ("YjAwMDE=", "hello ")] {
        // No `Authorization` header anywhere in this block: a blob client
        // never sends one, so the URL's own `sig` has to carry it.
        let staged = agent
            .put(&format!("{upload_url}&comp=block&blockid={block_id}"))
            .send(chunk)
            .expect("stage block");
        assert_eq!(staged.status(), 201);
    }
    let committed = agent
        .put(&format!("{upload_url}&comp=blocklist"))
        .send(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>\
             <Latest>YjAwMDE=</Latest><Latest>YjAwMDI=</Latest></BlockList>",
        )
        .expect("commit block list");
    assert_eq!(committed.status(), 201);

    // 3. FinalizeArtifact makes it listable.
    let finalized = twirp(
        &agent,
        &base,
        "FinalizeArtifact",
        serde_json::json!({
            "workflow_run_backend_id": SCOPE,
            "workflow_job_run_backend_id": "job-1",
            "name": "build-output",
            "size": "11",
        }),
    );
    assert_eq!(
        finalized.get("ok").and_then(serde_json::Value::as_bool),
        Some(true)
    );

    // 4. The download side, as a later job in the same run performs it.
    let listed = twirp(
        &agent,
        &base,
        "ListArtifacts",
        serde_json::json!({
            "workflow_run_backend_id": SCOPE,
            "workflow_job_run_backend_id": "job-2",
        }),
    );
    let artifacts = listed
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
        .expect("artifacts array");
    assert_eq!(artifacts.len(), 1);
    assert_eq!(
        artifacts[0].get("name").and_then(serde_json::Value::as_str),
        Some("build-output")
    );

    let signed = twirp(
        &agent,
        &base,
        "GetSignedArtifactURL",
        serde_json::json!({
            "workflow_run_backend_id": SCOPE,
            "workflow_job_run_backend_id": "job-2",
            "name": "build-output",
        }),
    );
    let download_url = signed
        .get("signedUrl")
        .and_then(serde_json::Value::as_str)
        .expect("signed_url");

    let mut downloaded = agent.get(download_url).call().expect("download");
    assert_eq!(
        downloaded.body_mut().read_to_string().expect("body"),
        "hello world",
        "the blocks assembled in the order the commit listed, not the order they arrived"
    );

    fixture.shim.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_small_artifact_may_arrive_as_one_unstaged_put() {
    let fixture = start().await;
    let base = fixture.base.clone();
    let agent = agent();

    // The Azure SDK short-circuits a small payload rather than staging a
    // single block; requiring the staged form would make small artifacts fail
    // where large ones succeed.
    let created = twirp(
        &agent,
        &base,
        "CreateArtifact",
        serde_json::json!({ "workflow_run_backend_id": SCOPE, "name": "tiny" }),
    );
    let upload_url = created
        .get("signedUploadUrl")
        .and_then(serde_json::Value::as_str)
        .expect("upload url")
        .to_string();

    let put = agent
        .put(&upload_url)
        .header("x-ms-blob-type", "BlockBlob")
        .send("small")
        .expect("whole-blob put");
    assert_eq!(put.status(), 201);

    twirp(
        &agent,
        &base,
        "FinalizeArtifact",
        serde_json::json!({ "workflow_run_backend_id": SCOPE, "name": "tiny", "size": "5" }),
    );

    let signed = twirp(
        &agent,
        &base,
        "GetSignedArtifactURL",
        serde_json::json!({ "workflow_run_backend_id": SCOPE, "name": "tiny" }),
    );
    let mut body = agent
        .get(
            signed
                .get("signedUrl")
                .and_then(serde_json::Value::as_str)
                .expect("url"),
        )
        .call()
        .expect("download");
    assert_eq!(body.body_mut().read_to_string().expect("body"), "small");

    fixture.shim.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_artifact_is_scoped_to_its_run() {
    let fixture = start().await;
    let base = fixture.base.clone();
    let agent = agent();

    let created = twirp(
        &agent,
        &base,
        "CreateArtifact",
        serde_json::json!({ "workflow_run_backend_id": SCOPE, "name": "scoped" }),
    );
    let url = created
        .get("signedUploadUrl")
        .and_then(serde_json::Value::as_str)
        .expect("url")
        .to_string();
    agent.put(&url).send("x").expect("put");
    twirp(
        &agent,
        &base,
        "FinalizeArtifact",
        serde_json::json!({ "workflow_run_backend_id": SCOPE, "name": "scoped", "size": "1" }),
    );

    // A different run must not see it.
    let other = twirp(
        &agent,
        &base,
        "ListArtifacts",
        serde_json::json!({ "workflow_run_backend_id": "a-different-run" }),
    );
    assert!(
        other
            .get("artifacts")
            .and_then(serde_json::Value::as_array)
            .expect("array")
            .is_empty(),
        "artifacts are scoped to the run that produced them"
    );

    fixture.shim.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unfinalized_upload_is_never_listed_or_downloadable() {
    let fixture = start().await;
    let base = fixture.base.clone();
    let agent = agent();

    let created = twirp(
        &agent,
        &base,
        "CreateArtifact",
        serde_json::json!({ "workflow_run_backend_id": SCOPE, "name": "half" }),
    );
    let url = created
        .get("signedUploadUrl")
        .and_then(serde_json::Value::as_str)
        .expect("url")
        .to_string();
    agent.put(&url).send("partial").expect("put");
    // No FinalizeArtifact -- the shape a killed job leaves behind.

    let listed = twirp(
        &agent,
        &base,
        "ListArtifacts",
        serde_json::json!({ "workflow_run_backend_id": SCOPE }),
    );
    assert!(
        listed
            .get("artifacts")
            .and_then(serde_json::Value::as_array)
            .expect("array")
            .is_empty(),
        "an interrupted upload must never look like a complete artifact"
    );

    fixture.shim.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_routes_refuse_a_request_without_the_runtime_token() {
    let fixture = start().await;
    let base = fixture.base.clone();
    let agent = agent();

    let unauthenticated = agent
        .post(&format!("{base}twirp/{SERVICE}/ListArtifacts"))
        .send_json(serde_json::json!({ "workflow_run_backend_id": SCOPE }));
    let status = match unauthenticated {
        Ok(response) => response.status().as_u16(),
        Err(ureq::Error::StatusCode(code)) => code,
        Err(other) => panic!("expected a status error, got {other:?}"),
    };
    assert_eq!(status, 401);

    fixture.shim.shutdown().await;
}
