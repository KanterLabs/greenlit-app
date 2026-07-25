//! The `_apis/artifactcache` routes: exactly the five calls `@actions/cache`
//! makes, and nothing else.
//!
//! Shapes are transcribed from the current toolkit source
//! (`packages/cache/src/internal/cacheHttpClient.ts`), not from memory:
//!
//! | Call | Route | Body / result |
//! |---|---|---|
//! | `getCacheEntry` | `GET cache?keys=<csv>&version=<v>` | 204 on miss; `{cacheKey, archiveLocation}` on hit |
//! | `reserveCache` | `POST caches` | `{key, version, cacheSize}` → `{cacheId}` |
//! | `uploadChunk` | `PATCH caches/<id>` | raw bytes at `Content-Range` |
//! | `commitCache` | `POST caches/<id>` | `{size}` |
//! | *(download)* | `GET blobs/<id>` | the archive bytes |
//!
//! The client builds every URL as `` `${baseUrl}_apis/artifactcache/${resource}` ``,
//! so `ACTIONS_CACHE_URL` must be handed to the job with a trailing slash.
//!
//! The download route is Greenlit's own: the hosted service answers a hit
//! with an `archiveLocation` pointing at blob storage, and the client simply
//! GETs whatever URL it is given. Pointing it back at this shim keeps the
//! bytes on this machine.
//!
//! Every route requires the run's `ACTIONS_RUNTIME_TOKEN` as a bearer token.
//! `PHASE-4-environment.md` calls this the "authenticated host-control
//! boundary": the shim is reachable from the job network, so an unauthenticated
//! route would let any container on that network read another run's cache.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use serde::{Deserialize, Serialize};

use crate::cache::CacheStore;
use crate::error::StoreError;
use crate::server::state::ShimState;

/// `GET cache?keys=…&version=…` — the ordered key list, comma separated.
#[derive(Debug, Deserialize)]
struct LookupQuery {
    keys: String,
    version: String,
}

/// The hit body. `archiveLocation` is where the client GETs the bytes.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CacheEntry {
    cache_key: String,
    archive_location: String,
}

/// `POST caches` — `cacheSize` is advisory and deliberately ignored; the
/// committed size is whatever the commit call reports.
#[derive(Debug, Deserialize)]
struct ReserveRequest {
    key: String,
    version: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReserveResponse {
    cache_id: i64,
}

/// `POST caches/<id>` — the commit body.
#[derive(Debug, Deserialize)]
struct CommitRequest {
    size: u64,
}

/// Builds the `_apis/artifactcache` routes onto `router`.
pub(crate) fn routes(router: Router<Arc<ShimState>>) -> Router<Arc<ShimState>> {
    router
        .route("/_apis/artifactcache/cache", get(lookup))
        .route("/_apis/artifactcache/caches", post(reserve))
        .route(
            "/_apis/artifactcache/caches/{id}",
            patch(upload).post(commit),
        )
        .route("/_apis/artifactcache/blobs/{id}", get(download))
}

async fn lookup(
    State(state): State<Arc<ShimState>>,
    headers: HeaderMap,
    Query(query): Query<LookupQuery>,
) -> Response {
    if let Err(denied) = state.authorize(&headers) {
        return denied.into_response();
    }

    // The client sends `[key, ...restoreKeys]` joined by commas.
    let keys: Vec<String> = query.keys.split(',').map(str::to_string).collect();

    match state.cache().lookup(&keys, &query.version) {
        // A miss is 204 No Content, which is what `getCacheEntry` checks for
        // before returning null. It is not an error.
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(restored)) => axum::Json(CacheEntry {
            cache_key: restored.key,
            archive_location: state.blob_url(restored.id),
        })
        .into_response(),
        Err(error) => failure(&error),
    }
}

async fn reserve(
    State(state): State<Arc<ShimState>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<ReserveRequest>,
) -> Response {
    if let Err(denied) = state.authorize(&headers) {
        return denied.into_response();
    }

    match state.cache().reserve(&request.key, &request.version) {
        Ok(cache_id) => (
            StatusCode::CREATED,
            axum::Json(ReserveResponse { cache_id }),
        )
            .into_response(),
        // The hosted service answers a duplicate key with 409, which
        // `actions/cache` logs and treats as a successful no-op ("another
        // job saved it first") rather than failing the step.
        Err(StoreError::AlreadyReserved { .. }) => StatusCode::CONFLICT.into_response(),
        Err(error) => failure(&error),
    }
}

async fn upload(
    State(state): State<Arc<ShimState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(denied) = state.authorize(&headers) {
        return denied.into_response();
    }

    let Some(offset) = content_range_start(&headers) else {
        return (
            StatusCode::BAD_REQUEST,
            "a cache upload chunk must carry a Content-Range header",
        )
            .into_response();
    };

    match state.cache().write_chunk(id, offset, &body) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(StoreError::UnknownReservation { .. }) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failure(&error),
    }
}

async fn commit(
    State(state): State<Arc<ShimState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<CommitRequest>,
) -> Response {
    if let Err(denied) = state.authorize(&headers) {
        return denied.into_response();
    }

    match state.cache().commit(id, request.size) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(StoreError::UnknownReservation { .. }) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failure(&error),
    }
}

/// The `?sig=` a blob URL carries in place of a bearer header.
#[derive(Debug, Deserialize)]
struct BlobQuery {
    sig: Option<String>,
}

async fn download(
    State(state): State<Arc<ShimState>>,
    Path(id): Path<i64>,
    Query(query): Query<BlobQuery>,
) -> Response {
    // `actions/cache` fetches `archiveLocation` with a bare `HttpClient` that
    // sends no `Authorization`. Requiring one here turned every cache *hit*
    // into a failed download, which the action reports as an ordinary miss --
    // so the cache looked like it never restored anything.
    if !state.signature_matches(query.sig.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let store: &CacheStore = state.cache();
    let Ok(path) = store.blob_path(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Parses the inclusive start offset out of `Content-Range: bytes <s>-<e>/<n>`.
///
/// `uploadChunk` formats exactly that shape (`getContentRange` in the toolkit
/// source), so this parses the one form the client sends rather than the full
/// RFC 9110 grammar; anything else is rejected as a bad request instead of
/// being guessed at.
fn content_range_start(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let rest = value.strip_prefix("bytes ")?;
    let (start, _) = rest.split_once('-')?;
    start.trim().parse().ok()
}

/// A store failure is the shim's own fault, not the workflow's: answer 500
/// and record why. The message reaches the run's log through the shim's
/// tracing span, never through the response body, so a workflow step cannot
/// echo a host path back into its own output.
fn failure(error: &StoreError) -> Response {
    tracing::error!(target: "greenlit_store::shim", %error, "cache shim request failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
