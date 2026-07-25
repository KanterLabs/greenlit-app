//! Resumable HTTP materialization for immutable CAS objects.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use super::{CasError, ObjectDigest};

/// One exact immutable HTTP source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpFetch {
    /// Locked download URL.
    pub url: String,
    /// User-Agent value sent to the source.
    pub user_agent: String,
    /// Maximum accepted complete-object size.
    pub max_bytes: u64,
    /// When true, only an already verified CAS hit is allowed.
    pub offline: bool,
}

pub(super) fn download(
    partial: &Path,
    offset: u64,
    digest: &ObjectDigest,
    fetch: &HttpFetch,
) -> Result<(), CasError> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = config.into();
    let mut request = agent
        .get(&fetch.url)
        .header("User-Agent", &fetch.user_agent);
    if offset > 0 {
        request = request.header("Range", &format!("bytes={offset}-"));
    }
    let mut response = request
        .call()
        .map_err(|error| http_error(digest, fetch, error.to_string()))?;
    let status = response.status().as_u16();
    if status != 200 && status != 206 {
        return Err(http_error(digest, fetch, format!("HTTP status {status}")));
    }

    let append = offset > 0 && status == 206;
    if append {
        let returned = response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let expected = format!("bytes {offset}-");
        if !returned.starts_with(&expected) {
            return Err(CasError::ResumeMismatch {
                digest: digest.clone(),
                source_url: fetch.url.clone(),
                requested: offset,
                returned: returned.to_string(),
            });
        }
    }

    let starting_size = if append { offset } else { 0 };
    if starting_size > fetch.max_bytes {
        return Err(too_large(digest, fetch));
    }
    let mut output = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!append)
        .open(partial)
        .map_err(|source| CasError::Io {
            path: partial.display().to_string(),
            source,
        })?;
    if append {
        output
            .seek(SeekFrom::End(0))
            .map_err(|source| CasError::Io {
                path: partial.display().to_string(),
                source,
            })?;
    }

    let mut total = starting_size;
    let mut buffer = [0_u8; 64 * 1024];
    let mut body = response.body_mut().as_reader();
    loop {
        let read = body
            .read(&mut buffer)
            .map_err(|error| http_error(digest, fetch, error.to_string()))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| too_large(digest, fetch))?;
        if total > fetch.max_bytes {
            return Err(too_large(digest, fetch));
        }
        output
            .write_all(&buffer[..read])
            .map_err(|source| CasError::Io {
                path: partial.display().to_string(),
                source,
            })?;
    }
    output.sync_all().map_err(|source| CasError::Io {
        path: partial.display().to_string(),
        source,
    })
}

fn http_error(digest: &ObjectDigest, fetch: &HttpFetch, message: String) -> CasError {
    CasError::Http {
        digest: digest.clone(),
        source_url: fetch.url.clone(),
        message,
    }
}

fn too_large(digest: &ObjectDigest, fetch: &HttpFetch) -> CasError {
    CasError::ResponseTooLarge {
        digest: digest.clone(),
        source_url: fetch.url.clone(),
        limit: fetch.max_bytes,
    }
}
