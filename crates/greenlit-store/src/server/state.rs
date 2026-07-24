//! Shared state for the shim's routes: the stores it serves, the token it
//! requires, and the base URL it hands back to clients.

use axum::http::{HeaderMap, StatusCode, header};

use crate::cache::CacheStore;

/// Everything the shim's handlers need, shared behind an `Arc`.
#[derive(Debug)]
pub struct ShimState {
    cache: CacheStore,
    token: String,
    base_url: String,
}

impl ShimState {
    /// Builds the state for one run.
    ///
    /// `base_url` is how the *job container* addresses this shim (the
    /// Greenlit bridge gateway plus the bound port), not how the host does:
    /// it is echoed back to the client as the `archiveLocation` a cache hit
    /// is downloaded from, so it must resolve from inside the container.
    /// `token` is the run's `ACTIONS_RUNTIME_TOKEN`.
    #[must_use]
    pub fn new(cache: CacheStore, token: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            cache,
            token: token.into(),
            base_url: base_url.into(),
        }
    }

    /// The cache store this shim serves.
    #[must_use]
    pub fn cache(&self) -> &CacheStore {
        &self.cache
    }

    /// The URL a client downloads committed entry `id` from.
    #[must_use]
    pub fn blob_url(&self, id: i64) -> String {
        format!(
            "{}_apis/artifactcache/blobs/{id}",
            with_trailing_slash(&self.base_url)
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

#[cfg(test)]
mod tests {
    use super::{ShimState, with_trailing_slash};
    use crate::cache::CacheStore;
    use axum::http::{HeaderMap, HeaderValue, header};

    fn state() -> ShimState {
        ShimState::new(
            CacheStore::at("/tmp/unused"),
            "secret",
            "http://10.0.0.1:8080",
        )
    }

    #[test]
    fn a_matching_bearer_token_is_accepted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert!(state().authorize(&headers).is_ok());
    }

    #[test]
    fn a_wrong_or_missing_token_is_refused() {
        assert!(
            state().authorize(&HeaderMap::new()).is_err(),
            "no header at all must not authorize"
        );

        for presented in ["Bearer wrong", "Bearer ", "secret", "Basic secret"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(presented).expect("test header"),
            );
            assert!(
                state().authorize(&headers).is_err(),
                "{presented:?} must not authorize"
            );
        }
    }

    #[test]
    fn the_blob_url_is_addressable_from_the_container() {
        // The client concatenates onto the base, so the slash matters.
        assert_eq!(
            state().blob_url(7),
            "http://10.0.0.1:8080/_apis/artifactcache/blobs/7"
        );
        assert_eq!(with_trailing_slash("http://x/"), "http://x/");
        assert_eq!(with_trailing_slash("http://x"), "http://x/");
    }
}
