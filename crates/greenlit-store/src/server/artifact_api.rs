//! The artifact routes: five twirp methods plus the blob transfer they hand
//! the client off to.
//!
//! Shapes transcribed from the current toolkit source, not from memory:
//!
//! * `packages/artifact/src/generated/results/api/v1/artifact.twirp-client.ts`
//!   declares exactly five methods on
//!   `github.actions.results.api.v1.ArtifactService` — `CreateArtifact`,
//!   `FinalizeArtifact`, `ListArtifacts`, `GetSignedArtifactURL`,
//!   `DeleteArtifact` — reached at `POST {base}/twirp/{service}/{method}`
//!   with a JSON body (`artifact-twirp-client.ts`:
//!   ``new URL(`/twirp/${service}/${method}`, this.baseUrl)``).
//! * `packages/artifact/src/internal/upload/blob-upload.ts` uploads through
//!   `blockBlobClient.uploadStream(...)`, i.e. Azure Block Blob.
//!
//! # Why an Azure dialect appears in a local tool
//!
//! On a hosted runner `CreateArtifact` answers with a `signedUploadUrl`
//! pointing at Azure blob storage, and the action uploads to *whatever URL it
//! is handed* using the Azure SDK. Greenlit hands it a URL pointing back
//! here, so this shim has to answer in that dialect. Nothing contacts Azure
//! and no credentials exist; it is a wire format, not a dependency.
//!
//! The three shapes the SDK produces:
//!
//! | Request | Meaning |
//! |---|---|
//! | `PUT …?comp=block&blockid=<b64>` | stage one chunk |
//! | `PUT …?comp=blocklist` | commit an ordering (XML body) |
//! | `PUT …` (no `comp`) | write the whole blob in one shot |
//!
//! The last is accepted because the SDK short-circuits small payloads rather
//! than staging a single block, and requiring the staged form would make
//! small artifacts fail in a way large ones do not.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use serde::{Deserialize, Serialize};

use crate::artifacts::blocklist;
use crate::error::StoreError;
use crate::server::state::ShimState;

/// The twirp service every artifact call is addressed to.
const SERVICE: &str = "github.actions.results.api.v1.ArtifactService";

/// Requests are snake_case, responses are camelCase. This asymmetry is real,
/// not an oversight.
///
/// The generated client serializes each request with
/// `Request.toJson(request, { useProtoFieldName: true })` — proto field
/// names, so `workflow_run_backend_id`. It parses each response with
/// `Response.fromJson(data, { ignoreUnknownFields: true })` and **no**
/// `useProtoFieldName`, so it expects protobuf-JSON's default lowerCamelCase
/// — `signedUploadUrl`.
///
/// `ignoreUnknownFields` is what makes getting this wrong so quiet: a
/// snake_case response is not rejected, it is *ignored*, leaving the field at
/// its default. `upload-artifact` then calls `new BlobClient("")` and throws
/// with an empty message, having never sent a single byte to the shim. That
/// is precisely how the `full-ci` fixture failed, and no synthetic test
/// caught it because those construct responses by hand.
///
/// The fields Greenlit reads from a twirp request.
///
/// The client sends more than this — expiry, version, hashes — which are
/// accepted and ignored rather than rejected, so a client-side addition does
/// not break the shim. `serde` ignores unknown fields by default, which is
/// the behavior wanted here.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ArtifactRequest {
    /// The run scope, treated as an opaque identifier.
    #[serde(default)]
    workflow_run_backend_id: String,
    /// The artifact name, for the calls that carry one.
    #[serde(default)]
    name: Option<String>,
    /// `FinalizeArtifact`'s reported size, as a JSON string or number.
    #[serde(default)]
    size: Option<serde_json::Value>,
    /// `ListArtifacts`' optional name filter.
    #[serde(default)]
    name_filter: Option<String>,
    /// `DeleteArtifact`'s target.
    #[serde(default)]
    id_filter: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateResponse {
    ok: bool,
    signed_upload_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FinalizeResponse {
    ok: bool,
    artifact_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListedArtifact {
    workflow_run_backend_id: String,
    workflow_job_run_backend_id: String,
    database_id: String,
    name: String,
    size: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResponse {
    artifacts: Vec<ListedArtifact>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SignedUrlResponse {
    signed_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteResponse {
    ok: bool,
    artifact_id: String,
}

/// Azure's `?comp=` selector plus the block id it carries.
#[derive(Debug, Deserialize, Default)]
struct BlobQuery {
    comp: Option<String>,
    blockid: Option<String>,
    /// The per-run URL signature. Blob clients send no `Authorization`
    /// header, so this is what authorizes the request.
    sig: Option<String>,
}

/// Builds the artifact routes onto `router`.
pub(crate) fn routes(router: Router<Arc<ShimState>>) -> Router<Arc<ShimState>> {
    router
        .route(&format!("/twirp/{SERVICE}/{{method}}"), post(twirp))
        .route("/greenlit/artifacts/{id}", put(blob_put).get(blob_get))
}

/// Dispatches one twirp call by method name.
async fn twirp(
    State(state): State<Arc<ShimState>>,
    Path(method): Path<String>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<ArtifactRequest>,
) -> Response {
    if let Err(denied) = state.authorize(&headers) {
        return denied.into_response();
    }

    let store = state.artifacts();
    let scope = &request.workflow_run_backend_id;

    match method.as_str() {
        "CreateArtifact" => {
            let Some(name) = request.name.as_deref() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            match store.create(scope, name) {
                Ok(id) => axum::Json(CreateResponse {
                    ok: true,
                    signed_upload_url: state.artifact_blob_url(id),
                })
                .into_response(),
                // A duplicate name within one run is refused by GitHub too;
                // `ok: false` is the twirp-level "no" the client checks.
                Err(StoreError::AlreadyReserved { .. }) => axum::Json(CreateResponse {
                    ok: false,
                    signed_upload_url: String::new(),
                })
                .into_response(),
                Err(error) => failure(&error),
            }
        }
        "FinalizeArtifact" => {
            let Some(name) = request.name.as_deref() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            // The id is not echoed back by the client, so it is recovered
            // from the staged entry matching this scope and name.
            let Some(id) = state.pending_artifact(scope, name) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let size = request.size.as_ref().and_then(number).unwrap_or(0);
            match store.finalize(id, size) {
                Ok(()) => axum::Json(FinalizeResponse {
                    ok: true,
                    artifact_id: id.to_string(),
                })
                .into_response(),
                Err(error) => failure(&error),
            }
        }
        "ListArtifacts" => match store.list(scope, request.name_filter.as_deref()) {
            Ok(artifacts) => axum::Json(ListResponse {
                artifacts: artifacts
                    .into_iter()
                    .map(|artifact| ListedArtifact {
                        workflow_run_backend_id: artifact.scope,
                        workflow_job_run_backend_id: String::new(),
                        database_id: artifact.id.to_string(),
                        name: artifact.name,
                        size: artifact.size.to_string(),
                    })
                    .collect(),
            })
            .into_response(),
            Err(error) => failure(&error),
        },
        "GetSignedArtifactURL" => {
            let Some(name) = request.name.as_deref() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            match store.list(scope, Some(name)) {
                Ok(found) => match found.first() {
                    Some(artifact) => axum::Json(SignedUrlResponse {
                        signed_url: state.artifact_blob_url(artifact.id),
                    })
                    .into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                },
                Err(error) => failure(&error),
            }
        }
        "DeleteArtifact" => {
            let target = request
                .id_filter
                .as_ref()
                .and_then(number)
                .and_then(|value| i64::try_from(value).ok());
            let id = match target {
                Some(id) => id,
                None => {
                    let Some(name) = request.name.as_deref() else {
                        return StatusCode::BAD_REQUEST.into_response();
                    };
                    match store
                        .list(scope, Some(name))
                        .map(|found| found.first().map(|a| a.id))
                    {
                        Ok(Some(id)) => id,
                        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
                        Err(error) => return failure(&error),
                    }
                }
            };
            match store.delete(id) {
                Ok(()) => axum::Json(DeleteResponse {
                    ok: true,
                    artifact_id: id.to_string(),
                })
                .into_response(),
                Err(StoreError::UnknownReservation { .. }) => StatusCode::NOT_FOUND.into_response(),
                Err(error) => failure(&error),
            }
        }
        // An unknown method is the client and the shim disagreeing about the
        // service, which is worth surfacing rather than silently accepting.
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The Azure Block Blob write surface.
async fn blob_put(
    State(state): State<Arc<ShimState>>,
    Path(id): Path<i64>,
    Query(query): Query<BlobQuery>,
    body: axum::body::Bytes,
) -> Response {
    if !state.signature_matches(query.sig.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let store = state.artifacts();
    let result = match query.comp.as_deref() {
        Some("block") => match query.blockid.as_deref() {
            Some(block_id) => store.stage_block(id, block_id, &body),
            None => return StatusCode::BAD_REQUEST.into_response(),
        },
        Some("blocklist") => {
            let document = String::from_utf8_lossy(&body);
            let ids = blocklist::parse(&document);
            if ids.is_empty() {
                return StatusCode::BAD_REQUEST.into_response();
            }
            store.commit_blocks(id, &ids)
        }
        // No `comp`: the whole blob in one request.
        _ => store.put_whole(id, &body),
    };

    match result {
        // Azure answers a successful write with 201 plus a small set of
        // response headers its SDK's generated deserializer reads. Returning
        // a bare 201 makes the client throw with an *empty* message, which is
        // exactly what the full-ci fixture hit -- so the shim answers the way
        // the service it is standing in for answers.
        Ok(()) => (
            StatusCode::CREATED,
            [
                ("x-ms-request-id", request_id()),
                ("x-ms-version", "2021-08-06".to_string()),
                ("x-ms-request-server-encrypted", "false".to_string()),
                ("etag", format!("\"{}\"", request_id())),
                ("last-modified", "Thu, 01 Jan 1970 00:00:00 GMT".to_string()),
            ],
        )
            .into_response(),
        Err(StoreError::UnknownReservation { .. }) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failure(&error),
    }
}

/// Downloads a finalized artifact's bytes.
async fn blob_get(
    State(state): State<Arc<ShimState>>,
    Path(id): Path<i64>,
    Query(query): Query<BlobQuery>,
) -> Response {
    if !state.signature_matches(query.sig.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(path) = state.artifacts().blob_path(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// An opaque request identifier, in the shape Azure returns.
///
/// The value is never interpreted by anything; the client only requires that
/// the header be present and well formed.
fn request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("00000000-0000-0000-0000-{counter:012x}")
}

/// Reads a JSON field the client may send as a number or a string.
///
/// The generated twirp client encodes 64-bit integers as strings, so a field
/// that is a number in the proto arrives quoted.
fn number(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// A store failure is the shim's fault, not the workflow's.
fn failure(error: &StoreError) -> Response {
    tracing::error!(target: "greenlit_store::shim", %error, "artifact shim request failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
