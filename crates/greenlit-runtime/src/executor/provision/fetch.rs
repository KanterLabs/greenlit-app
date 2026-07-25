//! Fetching and caching the runner-images manifest.
//!
//! # The pin
//!
//! `actions/runner-images` is pinned to one commit. Following `main` would
//! mean two `litci run` invocations a week apart provisioning different
//! versions of the same tool from the same workflow, which is the opposite of
//! the fidelity claim. Re-pinning is a deliberate act, exactly as it is for
//! the Node runtime bundles in `crate::executor::actions::node_runtime`.
//!
//! **Pinned commit:** `e986db797519f06a2e5e53701a715cfa4c1545e8`
//! (`actions/runner-images`, committed 2026-07-24).
//!
//! # Per-label, never shared
//!
//! `PHASE-4-environment.md`: 24.04 and 22.04 must never borrow each other's
//! recipes. Each label reads its own `toolset-<version>.json` and its own
//! `Ubuntu<version>-Readme.md`, and caches under its own directory keyed by
//! the pinned commit — so a re-pin cannot silently reuse the previous
//! commit's answers either.
//!
//! # Caching
//!
//! `~/.litci/runner-images/<sha>/<label>/`, installed atomically the way
//! `greenlit_actions::ActionStore` installs an action: fetch into a
//! `.tmp-…` sibling, then rename. "Destination already exists" means another
//! `litci` won the race, which is a hit rather than an error.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::manifest::Manifest;
use crate::executor::ExecError;

/// The pinned `actions/runner-images` commit. See the module docs before
/// changing this — it is a deliberate re-pin, never an incidental edit.
pub(crate) const PINNED_SHA: &str = "e986db797519f06a2e5e53701a715cfa4c1545e8";

/// Bounds a stalled connection rather than reflecting an expected latency.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The toolset JSON is tens of KiB and the Readme low hundreds; this is
/// generous headroom that still bounds what a compromised mirror could make
/// the host hold.
const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

/// The two upstream paths for one runner label.
fn document_urls(version: &str) -> (String, String) {
    let base = format!(
        "https://raw.githubusercontent.com/actions/runner-images/{PINNED_SHA}/images/ubuntu"
    );
    (
        format!("{base}/toolsets/toolset-{}.json", version.replace('.', "")),
        format!("{base}/Ubuntu{}-Readme.md", version.replace('.', "")),
    )
}

/// Where one label's cached manifest lives.
fn cache_dir(root: &Path, version: &str) -> PathBuf {
    root.join("runner-images").join(PINNED_SHA).join(version)
}

/// Loads the manifest for `version` (`"24.04"`/`"22.04"`), fetching it only
/// on a cache miss.
///
/// # Errors
/// Returns [`ExecError`] only when the documents can neither be read from the
/// cache nor fetched. A partial fetch never lands in the cache, so a killed
/// run cannot leave a half-manifest that a later run trusts.
pub(crate) async fn load(root: &Path, version: &str) -> Result<Manifest, ExecError> {
    let dir = cache_dir(root, version);
    if let (Ok(toolset), Ok(readme)) = (
        std::fs::read_to_string(dir.join("toolset.json")),
        std::fs::read_to_string(dir.join("readme.md")),
    ) {
        return Ok(Manifest::parse(&toolset, &readme));
    }

    let (toolset_url, readme_url) = document_urls(version);
    let (toolset, readme) = fetch_pair(toolset_url, readme_url).await?;
    // Best effort: a manifest that could not be cached is still usable this
    // run, and failing here would turn a disk hiccup into a failed workflow.
    let _ = install(&dir, &toolset, &readme);
    Ok(Manifest::parse(&toolset, &readme))
}

/// Fetches both documents off the async runtime.
async fn fetch_pair(
    toolset_url: String,
    readme_url: String,
) -> Result<(String, String), ExecError> {
    tokio::task::spawn_blocking(move || {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Ok((get(&agent, &toolset_url)?, get(&agent, &readme_url)?))
    })
    .await
    .map_err(|error| ExecError::Infrastructure {
        message: format!("the runner-images manifest fetch did not complete: {error}"),
        fix: "retry".to_string(),
    })?
}

/// One bounded GET.
fn get(agent: &ureq::Agent, url: &str) -> Result<String, ExecError> {
    let failure = |detail: String| ExecError::Infrastructure {
        message: format!("could not read the runner-images manifest: {detail}"),
        fix: "check network connectivity and retry; Greenlit needs it once per runner label \
              and caches it afterwards"
            .to_string(),
    };

    let mut response = agent
        .get(url)
        .header(
            "User-Agent",
            concat!("greenlit-litci/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|error| failure(error.to_string()))?;
    let status = response.status();
    let mut body = response
        .body_mut()
        .with_config()
        .limit(MAX_DOCUMENT_BYTES)
        .read_to_string()
        .map_err(|error| failure(error.to_string()))?;
    if !status.is_success() {
        // Never retain more of a remote's response than is needed to report it.
        body.truncate(4096.min(body.len()));
        return Err(failure(format!(
            "HTTP {}: {}",
            status.as_u16(),
            body.trim()
        )));
    }
    Ok(body)
}

/// Installs both documents atomically, so a killed fetch is never a hit.
fn install(dir: &Path, toolset: &str, readme: &str) -> std::io::Result<()> {
    let parent = dir.parent().unwrap_or(dir);
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&staging)?;
    std::fs::write(staging.join("toolset.json"), toolset)?;
    std::fs::write(staging.join("readme.md"), readme)?;
    match std::fs::rename(&staging, dir) {
        Ok(()) => Ok(()),
        // Another `litci` got there first; that is a hit, not a failure.
        Err(_) if dir.is_dir() => {
            let _ = std::fs::remove_dir_all(&staging);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PINNED_SHA, cache_dir, document_urls};
    use std::path::Path;

    #[test]
    fn each_label_reads_its_own_documents() {
        // The requirement is explicit: 24.04 and 22.04 must never borrow each
        // other's recipes.
        let (toolset_2404, readme_2404) = document_urls("24.04");
        let (toolset_2204, readme_2204) = document_urls("22.04");
        assert!(toolset_2404.ends_with("/toolsets/toolset-2404.json"));
        assert!(readme_2404.ends_with("/Ubuntu2404-Readme.md"));
        assert!(toolset_2204.ends_with("/toolsets/toolset-2204.json"));
        assert!(readme_2204.ends_with("/Ubuntu2204-Readme.md"));
        assert_ne!(toolset_2404, toolset_2204);
    }

    #[test]
    fn every_url_carries_the_pin_rather_than_a_branch() {
        let (toolset, readme) = document_urls("24.04");
        for url in [&toolset, &readme] {
            assert!(url.contains(PINNED_SHA), "{url} must be pinned");
            assert!(
                !url.contains("/main/"),
                "{url} follows a branch, so two runs a week apart would differ"
            );
        }
    }

    #[test]
    fn the_cache_is_keyed_by_pin_and_label() {
        let root = Path::new("/home/user/.litci");
        assert_eq!(
            cache_dir(root, "24.04"),
            root.join("runner-images").join(PINNED_SHA).join("24.04")
        );
        // A re-pin cannot silently reuse the previous commit's answers.
        assert_ne!(cache_dir(root, "24.04"), cache_dir(root, "22.04"));
    }

    #[test]
    fn the_pin_is_a_full_commit_sha() {
        assert_eq!(PINNED_SHA.len(), 40);
        assert!(PINNED_SHA.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
