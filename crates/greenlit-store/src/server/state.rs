//! Shared state for the shim's routes: the stores it serves, the token it
//! requires, and the base URL it hands back to clients.

use axum::http::{HeaderMap, StatusCode, header};

use crate::artifacts::ArtifactStore;
use crate::cache::CacheStore;

/// Everything the shim's handlers need, shared behind an `Arc`.
#[derive(Debug)]
pub struct ShimState {
    cache: CacheStore,
    artifacts: ArtifactStore,
    token: String,
    signature: String,
    base_url: String,
}

impl ShimState {
    /// Builds the state for one run.
    ///
    /// `base_url` is how the *job container* addresses this shim (the
    /// Greenlit bridge gateway plus the bound port), not how the host does:
    /// it is echoed back to the client as the `archiveLocation` a cache hit
    /// is downloaded from and as the `signedUploadUrl` an artifact is written
    /// to, so it must resolve from inside the container. `token` is the run's
    /// `ACTIONS_RUNTIME_TOKEN`.
    #[must_use]
    pub fn new(
        cache: CacheStore,
        artifacts: ArtifactStore,
        token: impl Into<String>,
        signature: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            cache,
            artifacts,
            token: token.into(),
            signature: signature.into(),
            base_url: base_url.into(),
        }
    }

    /// Whether a blob request carries this run's URL signature.
    ///
    /// Blob routes cannot be authorized by a bearer header, because the
    /// clients that reach them never send one. `@azure/storage-blob` treats a
    /// "signed" URL as self-authorizing and adds no `Authorization`, and
    /// `actions/cache` fetches its `archiveLocation` with a bare `HttpClient`.
    /// Requiring a header there produced a 401 that the Azure SDK surfaced as
    /// a `RestError` with an *empty* message — an upload that failed for a
    /// reason nothing printed.
    ///
    /// The real service solves this with a SAS in the URL, and so does this:
    /// a per-run signature in the query string, kept separate from the bearer
    /// token so the token itself never appears in a URL a client might log.
    #[must_use]
    pub fn signature_matches(&self, presented: Option<&str>) -> bool {
        presented.is_some_and(|value| constant_time_eq(value.as_bytes(), self.signature.as_bytes()))
    }

    /// The cache store this shim serves.
    #[must_use]
    pub fn cache(&self) -> &CacheStore {
        &self.cache
    }

    /// The artifact store this shim serves.
    #[must_use]
    pub fn artifacts(&self) -> &ArtifactStore {
        &self.artifacts
    }

    /// The id of the artifact currently being uploaded under `name`.
    #[must_use]
    pub fn pending_artifact(&self, scope: &str, name: &str) -> Option<i64> {
        self.artifacts.pending(scope, name)
    }

    /// The URL a client downloads committed cache entry `id` from.
    #[must_use]
    pub fn blob_url(&self, id: i64) -> String {
        format!(
            "{}_apis/artifactcache/blobs/{id}?sig={}",
            with_trailing_slash(&self.base_url),
            self.signature
        )
    }

    /// The URL an artifact's bytes are written to and read from.
    #[must_use]
    pub fn artifact_blob_url(&self, id: i64) -> String {
        format!(
            "{}greenlit/artifacts/{id}?sig={}",
            with_trailing_slash(&self.base_url),
            self.signature
        )
    }

    /// Rejects a request that does not carry the run's runtime token.
    ///
    /// The shim is reachable from the job network, so this is the only thing
    /// standing between one container and another run's cache. Comparison is
    /// constant-time in the length-equal case so a caller on the same bridge
    /// cannot recover the token byte by byte from response timing.
    ///
    /// # Errors
    /// Returns [`StatusCode::UNAUTHORIZED`] when the bearer token is absent
    /// or does not match. The status is returned rather than a built
    /// `Response` so the refusal stays a cheap `Copy` value on every
    /// request's hot path.
    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();

        if constant_time_eq(presented.as_bytes(), self.token.as_bytes()) {
            Ok(())
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

/// Equality that does not short-circuit on the first differing byte.
///
/// Lengths are compared first and unequal lengths return immediately, which
/// leaks only the token's length -- a fixed property of Greenlit's own token
/// generation, not a secret.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// The client builds URLs by concatenation, so the base must end in `/`.
fn with_trailing_slash(base: &str) -> String {
    if base.ends_with('/') {
        base.to_string()
    } else {
        format!("{base}/")
    }
}
