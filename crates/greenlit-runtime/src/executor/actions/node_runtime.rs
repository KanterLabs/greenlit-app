//! The pinned `actions/runner` Node runtime bundles: which release is
//! pinned, where node20/node24's standard (glibc) and Alpine (musl) `linux-
//! x64` bundles come from, their checksums, the content-addressed cache
//! those bundles are fetched into, and the in-container libc probe that
//! picks which variant a job container needs.
//!
//! # Pinned source
//!
//! `PHASE-3-actions.md`: "pin one released `actions/runner` version and its
//! checksums, obtain the same `externals/node20`/`externals/node24` bundles
//! ... from the runner's external-runtime packaging script." Pinned against
//! `actions/runner` release **v2.336.0**
//! (<https://github.com/actions/runner/releases/tag/v2.336.0>, verified via
//! the GitHub API during implementation). That release's
//! `src/Misc/externals.sh` (the runner's own packaging script) declares:
//!
//! ```text
//! NODE20_VERSION="20.20.2"
//! NODE24_VERSION="24.18.0"
//! ```
//!
//! and, for `linux-x64` (the only `PACKAGERUNTIME` v0 targets):
//!
//! ```text
//! acquireExternalTool "$NODE_URL/v${NODE20_VERSION}/node-v${NODE20_VERSION}-linux-x64.tar.gz" node20 fix_nested_dir
//! acquireExternalTool "$NODE_ALPINE_URL/v${NODE20_VERSION}/node-v${NODE20_VERSION}-alpine-x64.tar.gz" node20_alpine
//! acquireExternalTool "$NODE_URL/v${NODE24_VERSION}/node-v${NODE24_VERSION}-linux-x64.tar.gz" node24 fix_nested_dir
//! acquireExternalTool "$NODE_ALPINE_URL/v${NODE24_VERSION}/node-v${NODE24_VERSION}-alpine-x64.tar.gz" node24_alpine
//! ```
//! where `NODE_URL=https://nodejs.org/dist` and
//! `NODE_ALPINE_URL=https://github.com/actions/alpine_nodejs/releases/download`
//! — exactly the four URLs `bundle_spec` (below) returns. `fix_nested_dir` is the
//! runner's own flag for "the tarball wraps everything in one top-level
//! `node-vX.Y.Z-linux-x64/` directory, strip it" — passed for the standard
//! tarballs and not for the Alpine ones, matching this module's
//! `strip_top_level` field (confirmed directly: the Alpine archive's own
//! entries are already rooted, e.g. `./bin/node`, with no nested directory).
//!
//! Checksums: the standard (glibc) tarballs' `sha256` come straight from
//! nodejs.org's own published `SHASUMS256.txt` for each version. The Alpine
//! tarballs are GitHub release assets
//! (<https://github.com/actions/alpine_nodejs>) that nodejs.org/the release
//! itself does not publish a checksum for; their pinned `sha256` below was
//! computed directly from the exact release asset during implementation
//! (downloaded from the URL below, `sha256sum`'d, and the file discarded —
//! this is the source of trust for those two entries specifically, not a
//! third party's republished digest).
//!
//! Action runtimes are pinned independently from the runner profile here, per
//! `PHASE-3-actions.md`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use greenlit_actions::manifest::NodeVersion;
use greenlit_actions::store::FetchOutcome;
use greenlit_store::cas::{CasError, CasStore, HttpFetch, ObjectDigest};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::engine::{ContainerEngine, ExecOutputSink, ExecSpec};
use crate::error::RuntimeError;

/// The `actions/runner` release these bundles are pinned against (see module
/// docs for the verification source).
pub const PINNED_RUNNER_VERSION: &str = "2.336.0";

/// A bounded ceiling on a downloaded bundle's compressed size — the real
/// bundles are on the order of tens of megabytes; this guards a
/// misconfigured or hostile response rather than reflecting an expected
/// size.
const MAX_COMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
/// A ceiling on the bundle's decompressed content, generous against the real
/// (~120-220 MiB installed) bundles.
const MAX_EXTRACTED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_EXTRACTED_ENTRIES: usize = 200_000;

/// Which libc a job container's image links against — decides the standard
/// vs. Alpine Node bundle, matching GitHub's own runner rule (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeVariant {
    /// glibc — the ordinary `linux-x64` Node build.
    Standard,
    /// musl (Alpine) — the `actions/alpine_nodejs` build.
    Alpine,
}

/// One pinned Node runtime bundle: where to fetch it, its expected checksum,
/// and how to unpack it.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeBundleSpec {
    pub version: NodeVersion,
    pub variant: NodeVariant,
    pub url: &'static str,
    pub sha256: &'static str,
    /// Whether extraction must strip one common top-level directory
    /// (the standard tarballs' `node-vX.Y.Z-linux-x64/` wrapper) — see
    /// module docs' `fix_nested_dir` note.
    pub strip_top_level: bool,
}

impl RuntimeBundleSpec {
    /// The cache-key path segment this bundle lives under:
    /// `<node20|node24>/<standard|alpine>/<sha256>`.
    pub(crate) fn cache_key(&self) -> PathBuf {
        let version = match self.version {
            NodeVersion::Node20 => "node20",
            NodeVersion::Node24 => "node24",
        };
        let variant = match self.variant {
            NodeVariant::Standard => "standard",
            NodeVariant::Alpine => "alpine",
        };
        PathBuf::from(version).join(variant).join(self.sha256)
    }
}

/// The pinned bundle for `version`/`variant` — see module docs for the
/// source of every URL and checksum.
pub(crate) fn bundle_spec(version: NodeVersion, variant: NodeVariant) -> RuntimeBundleSpec {
    match (version, variant) {
        (NodeVersion::Node20, NodeVariant::Standard) => RuntimeBundleSpec {
            version,
            variant,
            url: "https://nodejs.org/dist/v20.20.2/node-v20.20.2-linux-x64.tar.gz",
            sha256: "19e56f0825510207dd904f087fe52faa0a4eb6b2aab5f0ea7a33830d04888b8b",
            strip_top_level: true,
        },
        (NodeVersion::Node20, NodeVariant::Alpine) => RuntimeBundleSpec {
            version,
            variant,
            url: "https://github.com/actions/alpine_nodejs/releases/download/v20.20.2/node-v20.20.2-alpine-x64.tar.gz",
            sha256: "f21a2253025a5d1a14332a0b1ed48871689c5ca9aa37a6141428944b75de7d91",
            strip_top_level: false,
        },
        (NodeVersion::Node24, NodeVariant::Standard) => RuntimeBundleSpec {
            version,
            variant,
            url: "https://nodejs.org/dist/v24.18.0/node-v24.18.0-linux-x64.tar.gz",
            sha256: "783130984963db7ba9cbd01089eaf2c2efb055c7c1693c943174b967b3050cb8",
            strip_top_level: true,
        },
        (NodeVersion::Node24, NodeVariant::Alpine) => RuntimeBundleSpec {
            version,
            variant,
            url: "https://github.com/actions/alpine_nodejs/releases/download/v24.18.0/node-v24.18.0-alpine-x64.tar.gz",
            sha256: "0103dd81376d57dcc2bcb39a13cfd6db19ab82f6c2c83a166e44d775f736d0d9",
            strip_top_level: false,
        },
    }
}

/// A failure fetching, verifying, or extracting a Node runtime bundle.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeBundleError {
    /// The download itself failed (network, HTTP status, timeout).
    #[error("could not download the pinned Node runtime bundle from {url}: {message}")]
    Download {
        /// The URL that was requested.
        url: String,
        /// A bounded, display-safe diagnostic.
        message: String,
    },
    /// The downloaded bytes did not match the pinned checksum.
    #[error(
        "downloaded Node runtime bundle from {url} does not match its pinned checksum (expected {expected}, got {actual}) — refusing to extract it"
    )]
    ChecksumMismatch {
        /// The URL that was requested.
        url: String,
        /// The pinned, expected digest.
        expected: String,
        /// The digest actually computed.
        actual: String,
    },
    /// Extracting the verified archive failed.
    #[error("could not extract the Node runtime bundle: {0}")]
    Extract(String),
    /// Installing the extracted bundle into its content-addressed cache
    /// directory failed.
    #[error("could not install the Node runtime bundle into {}: {message}", path.display())]
    Install {
        /// The destination path.
        path: PathBuf,
        /// The underlying I/O error's message.
        message: String,
    },
    /// The verified machine-wide content store rejected or could not fetch
    /// the exact locked bundle.
    #[error("could not materialize the pinned Node runtime bundle: {0}")]
    Content(#[source] CasError),
    /// A hard-coded pinned checksum is malformed.
    #[error("the pinned Node runtime identity is invalid: {0}")]
    InvalidIdentity(String),
}

/// The injected boundary that downloads one pinned bundle's raw (still
/// compressed, not yet checksum-verified) bytes.
///
/// Production code (`greenlit-app`) supplies [`HttpRuntimeBundleFetcher`].
/// Real bundle acceptance belongs to the capability-owning action-execution
/// gate rather than the portable test set.
#[async_trait]
pub trait RuntimeBundleFetcher: Send + Sync {
    /// Downloads `spec`'s bundle, returning its raw compressed bytes.
    ///
    /// # Errors
    /// Returns [`RuntimeBundleError::Download`] on any network/HTTP failure.
    async fn download(&self, spec: &RuntimeBundleSpec) -> Result<Vec<u8>, RuntimeBundleError>;
}

/// Downloads a bundle from its real pinned URL over HTTPS.
#[derive(Debug, Clone, Default)]
pub struct HttpRuntimeBundleFetcher {
    agent: Option<ureq::Agent>,
}

impl HttpRuntimeBundleFetcher {
    /// A fetcher using a fresh, default-configured HTTPS agent.
    #[must_use]
    pub fn new() -> Self {
        Self { agent: None }
    }

    fn agent(&self) -> ureq::Agent {
        self.agent.clone().unwrap_or_else(|| {
            let config = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(300)))
                .http_status_as_error(false)
                .build();
            config.into()
        })
    }
}

#[async_trait]
impl RuntimeBundleFetcher for HttpRuntimeBundleFetcher {
    async fn download(&self, spec: &RuntimeBundleSpec) -> Result<Vec<u8>, RuntimeBundleError> {
        let agent = self.agent();
        let url = spec.url.to_string();
        let url_for_join_error = url.clone();
        tokio::task::spawn_blocking(move || download_blocking(&agent, &url))
            .await
            .map_err(|join_error| RuntimeBundleError::Download {
                url: url_for_join_error,
                message: format!("download task did not complete: {join_error}"),
            })?
    }
}

fn download_blocking(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, RuntimeBundleError> {
    let error = |message: String| RuntimeBundleError::Download {
        url: url.to_string(),
        message,
    };
    let mut response = agent
        .get(url)
        .header(
            "User-Agent",
            concat!("greenlit-litci/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|e| error(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(error(format!("HTTP {}", status.as_u16())));
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_COMPRESSED_BYTES)
        .read_to_vec()
        .map_err(|e| error(e.to_string()))
}

/// The content-addressed cache of extracted Node runtime bundles,
/// `~/.litci/node-runtimes/<node20|node24>/<standard|alpine>/<sha256>/` —
/// mirrors [`greenlit_actions::store::ActionStore`]'s atomic-install pattern
/// exactly (fetch to a temp sibling, verify, extract, rename into place) so
/// a killed download never leaves a half-populated directory a later run
/// would mistake for a complete one.
#[derive(Debug, Clone)]
pub struct RuntimeStore {
    root: PathBuf,
    counters: std::sync::Arc<Counters>,
    cas: Option<CasStore>,
    offline: bool,
}

#[derive(Debug, Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
}

/// A running hit/miss tally, mirroring
/// [`greenlit_actions::store::StoreCounts`] for the same end-of-run
/// breakdown purpose (`PHASE-3-actions.md`: "surface the store hit/miss
/// counts into the run's end-of-run breakdown").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeStoreCounts {
    /// Bundles already present when asked for.
    pub hits: u64,
    /// Bundles fetched because they were not already present.
    pub misses: u64,
}

impl RuntimeStore {
    /// The default cache root under a home directory: `<home>/.litci/node-runtimes`.
    #[must_use]
    pub fn default_path_under(home_dir: &Path) -> PathBuf {
        home_dir.join(".litci").join("node-runtimes")
    }

    /// Opens a store backed by an explicit root — production wires
    /// [`Self::default_path_under`]; tests use an isolated temp directory.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            counters: std::sync::Arc::new(Counters::default()),
            cas: None,
            offline: false,
        }
    }

    /// Uses `cas` as the verified compressed-bundle backing store while
    /// retaining `root` for safely extracted runtime trees.
    #[must_use]
    pub fn with_cas(root: impl Into<PathBuf>, cas: CasStore, offline: bool) -> Self {
        Self {
            root: root.into(),
            counters: std::sync::Arc::new(Counters::default()),
            cas: Some(cas),
            offline,
        }
    }

    /// The current hit/miss tally across every `ensure_fetched` call
    /// made through this store (and any clone of it).
    #[must_use]
    pub fn counts(&self) -> RuntimeStoreCounts {
        RuntimeStoreCounts {
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
        }
    }

    /// Where `spec`'s extracted bundle lives (or would live).
    fn dir(&self, spec: &RuntimeBundleSpec) -> PathBuf {
        self.root.join(spec.cache_key())
    }

    /// Ensures `spec`'s bundle is extracted and present, fetching only on a
    /// miss.
    ///
    /// # Errors
    /// Returns [`RuntimeBundleError`] if the download, checksum, extraction,
    /// or install fails.
    pub(crate) async fn ensure_fetched(
        &self,
        spec: &RuntimeBundleSpec,
        fetcher: &dyn RuntimeBundleFetcher,
    ) -> Result<(PathBuf, FetchOutcome), RuntimeBundleError> {
        let dest = self.dir(spec);
        if dest.is_dir() {
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            return Ok((dest, FetchOutcome::Hit));
        }
        let parent = dest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root.clone());
        std::fs::create_dir_all(&parent).map_err(|source| RuntimeBundleError::Install {
            path: parent.clone(),
            message: source.to_string(),
        })?;
        let tmp = parent.join(temp_sibling_name());
        std::fs::create_dir_all(&tmp).map_err(|source| RuntimeBundleError::Install {
            path: tmp.clone(),
            message: source.to_string(),
        })?;

        let install_result = if let Some(cas) = &self.cas {
            fetch_from_cas_verify_extract(spec, cas, self.offline, &tmp).await
        } else {
            fetch_verify_extract(spec, fetcher, &tmp).await
        };
        if let Err(error) = install_result {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(error);
        }
        match std::fs::rename(&tmp, &dest) {
            Ok(()) => {
                self.counters.misses.fetch_add(1, Ordering::Relaxed);
                Ok((dest, FetchOutcome::Miss))
            }
            Err(_) if dest.is_dir() => {
                // Lost a race with a concurrent `litci` process fetching the
                // same bundle; that is just as good a hit.
                let _ = std::fs::remove_dir_all(&tmp);
                self.counters.hits.fetch_add(1, Ordering::Relaxed);
                Ok((dest, FetchOutcome::Hit))
            }
            Err(source) => {
                let _ = std::fs::remove_dir_all(&tmp);
                Err(RuntimeBundleError::Install {
                    path: dest,
                    message: source.to_string(),
                })
            }
        }
    }
}

async fn fetch_from_cas_verify_extract(
    spec: &RuntimeBundleSpec,
    cas: &CasStore,
    offline: bool,
    dest: &Path,
) -> Result<(), RuntimeBundleError> {
    let digest = ObjectDigest::parse(&format!("sha256:{}", spec.sha256))
        .map_err(|error| RuntimeBundleError::InvalidIdentity(error.to_string()))?;
    let fetch = HttpFetch {
        url: spec.url.to_string(),
        user_agent: concat!("greenlit-litci/", env!("CARGO_PKG_VERSION")).to_string(),
        max_bytes: MAX_COMPRESSED_BYTES,
        offline,
    };
    let store = cas.clone();
    let digest_for_task = digest.clone();
    tokio::task::spawn_blocking(move || store.ensure_http(&digest_for_task, &fetch))
        .await
        .map_err(|error| RuntimeBundleError::Download {
            url: spec.url.to_string(),
            message: format!("content task did not complete: {error}"),
        })?
        .map_err(RuntimeBundleError::Content)?;
    let path = cas
        .verified_path(&digest)
        .map_err(RuntimeBundleError::Content)?
        .ok_or_else(|| {
            RuntimeBundleError::Content(CasError::OfflineMissing {
                digest,
                source_url: spec.url.to_string(),
            })
        })?;
    let bytes = std::fs::read(&path).map_err(|source| RuntimeBundleError::Install {
        path,
        message: source.to_string(),
    })?;
    extract_bundle(&bytes, dest, spec.strip_top_level).map_err(RuntimeBundleError::Extract)
}

async fn fetch_verify_extract(
    spec: &RuntimeBundleSpec,
    fetcher: &dyn RuntimeBundleFetcher,
    dest: &Path,
) -> Result<(), RuntimeBundleError> {
    let bytes = fetcher.download(spec).await?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = hex_digest(&hasher.finalize());
    if actual != spec.sha256 {
        return Err(RuntimeBundleError::ChecksumMismatch {
            url: spec.url.to_string(),
            expected: spec.sha256.to_string(),
            actual,
        });
    }
    extract_bundle(&bytes, dest, spec.strip_top_level).map_err(RuntimeBundleError::Extract)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn temp_sibling_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".tmp-{}-{}-{nanos}", std::process::id(), counter)
}

/// Extracts a gzip-compressed tar bundle into `dest` (already created,
/// empty), optionally stripping one common top-level directory. Bounded and
/// path-traversal-safe like `greenlit-actions`' action-tarball extractor,
/// adapted for gzip input and the `strip_top_level` choice this module's
/// two bundle shapes need (see module docs' `fix_nested_dir` note).
fn extract_bundle(bytes: &[u8], dest: &Path, strip_top_level: bool) -> Result<(), String> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("could not read tar stream: {error}"))?;

    let mut total_bytes: u64 = 0;
    let mut count: usize = 0;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("could not read tar entry: {error}"))?;
        count += 1;
        if count > MAX_EXTRACTED_ENTRIES {
            return Err(format!(
                "bundle exceeds the {MAX_EXTRACTED_ENTRIES}-entry extraction limit"
            ));
        }
        total_bytes = total_bytes.saturating_add(entry.header().size().unwrap_or(0));
        if total_bytes > MAX_EXTRACTED_BYTES {
            return Err(format!(
                "bundle exceeds the {MAX_EXTRACTED_BYTES}-byte extraction limit"
            ));
        }

        let raw_path = entry
            .path()
            .map_err(|error| format!("could not read entry path: {error}"))?
            .into_owned();
        let mut components = raw_path.components();
        if strip_top_level {
            components.next();
        }
        let relative = safe_relative_path(components)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(&relative);

        match entry.header().entry_type() {
            EntryType::Directory => {
                std::fs::create_dir_all(&target)
                    .map_err(|error| format!("could not create {}: {error}", target.display()))?;
            }
            EntryType::Regular => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        format!("could not create {}: {error}", parent.display())
                    })?;
                }
                let mut file = std::fs::File::create(&target)
                    .map_err(|error| format!("could not write {}: {error}", target.display()))?;
                std::io::copy(&mut entry, &mut file)
                    .map_err(|error| format!("could not write {}: {error}", target.display()))?;
                apply_mode(&file, &entry, &target)?;
            }
            EntryType::Symlink => {
                let link_name = entry
                    .link_name()
                    .map_err(|error| format!("could not read symlink target: {error}"))?
                    .ok_or_else(|| "symlink entry has no target".to_string())?;
                reject_escaping_link(&relative, &link_name)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        format!("could not create {}: {error}", parent.display())
                    })?;
                }
                std::os::unix::fs::symlink(&link_name, &target).map_err(|error| {
                    format!("could not create symlink {}: {error}", target.display())
                })?;
            }
            // Hardlinks and special nodes are not part of a legitimate Node
            // distribution archive; skip rather than fail the whole bundle.
            _ => {}
        }
    }
    Ok(())
}

fn apply_mode(
    file: &std::fs::File,
    entry: &tar::Entry<'_, impl Read>,
    target: &Path,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let Ok(mode) = entry.header().mode() else {
        return Ok(());
    };
    file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777))
        .map_err(|error| format!("could not set permissions on {}: {error}", target.display()))
}

fn safe_relative_path(components: std::path::Components<'_>) -> Result<PathBuf, String> {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("tar entry has an unsafe (absolute or `..`) path".to_string());
            }
        }
    }
    Ok(result)
}

fn reject_escaping_link(relative: &Path, link_name: &Path) -> Result<(), String> {
    use std::path::Component;
    if link_name.is_absolute() {
        return Err("symlink entry has an absolute target".to_string());
    }
    let mut depth: i64 =
        i64::try_from(relative.components().count().saturating_sub(1)).unwrap_or(i64::MAX);
    for component in link_name.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err("symlink entry would escape the destination directory".to_string());
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("symlink entry has an absolute target".to_string());
            }
        }
    }
    Ok(())
}

/// Probes a running container's libc the same way the real `actions/runner`
/// does (`src/Runner.Worker/Handlers/StepHost.cs`,
/// `DetermineNodeRuntimeVersion`, pinned release — module docs): exec
/// `cat /etc/*release | grep ^ID` and look for `alpine` (case-insensitively)
/// in any line; anything else (including the exec itself failing, e.g. no
/// `/etc/*release` at all) is treated as the standard glibc build, matching
/// the runner's own fallthrough.
///
/// # Errors
/// Returns [`RuntimeError`] only if the engine itself could not be reached;
/// a non-zero probe exit or unreadable file is not an error, just "not
/// Alpine".
pub(crate) async fn detect_node_variant(
    engine: &dyn ContainerEngine,
    container: &str,
) -> Result<NodeVariant, RuntimeError> {
    let spec = ExecSpec {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat /etc/*release 2>/dev/null | grep ^ID".to_string(),
        ],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = CaptureLower::default();
    // A non-zero exit (e.g. no matching file at all) is exactly the "not
    // Alpine" case the runner itself falls through to, not an error.
    let _ = engine.exec(container, &spec, &mut sink).await?;
    Ok(if sink.text.contains("alpine") {
        NodeVariant::Alpine
    } else {
        NodeVariant::Standard
    })
}

/// In-container root every mounted Node runtime lives under:
/// `/greenlit/node/<node20|node24>/<standard|alpine>`.
const NODE_MOUNT_ROOT: &str = "/greenlit/node";

/// Ensures every Node runtime a job's resolved actions need is cached and
/// returns the read-only binds to add to that job's container, plus the
/// container-path mounts for each.
///
/// Mounts **both** the standard and Alpine variant of each needed version
/// (see `super::nodejs`/module docs: the job container's libc is only
/// knowable once it is already running, so which one is *used* is decided
/// at exec time by `detect_node_variant`/`ensure_variant`, not at bind
/// time) — the same tradeoff the real runner's own layout makes, which
/// ships both `externals/node20` and `externals/node20_alpine` side by side
/// in every job regardless of which one a given container turns out to
/// need.
///
/// # Errors
/// Returns [`RuntimeBundleError`] if any needed bundle's download,
/// checksum, or extraction fails.
pub(crate) async fn ensure_mounts(
    store: &RuntimeStore,
    fetcher: &dyn RuntimeBundleFetcher,
    specs: &dyn NodeBundleSpecs,
    needs_node20: bool,
    needs_node24: bool,
) -> Result<
    (
        Vec<crate::engine::BindMount>,
        super::nodejs::NodeRuntimeMounts,
    ),
    RuntimeBundleError,
> {
    let mut binds = Vec::new();
    let mut mounts = super::nodejs::NodeRuntimeMounts::default();

    for (needed, version) in [
        (needs_node20, NodeVersion::Node20),
        (needs_node24, NodeVersion::Node24),
    ] {
        if !needed {
            continue;
        }
        for variant in [NodeVariant::Standard, NodeVariant::Alpine] {
            let spec = specs.spec(version, variant);
            let (host_dir, _outcome) = store.ensure_fetched(&spec, fetcher).await?;
            let container_dir = format!("{NODE_MOUNT_ROOT}/{}", spec.cache_key().to_string_lossy());
            binds.push(crate::engine::BindMount {
                host_path: host_dir.to_string_lossy().into_owned(),
                container_path: container_dir.clone(),
                read_only: true,
            });
            mounts.set(version, variant, container_dir);
        }
    }
    Ok((binds, mounts))
}

/// The injected boundary that picks which [`RuntimeBundleSpec`] (URL,
/// checksum) to ensure for a given `(version, variant)`.
///
/// Production code (`greenlit-app`) wires [`PinnedNodeBundleSpecs`], the
/// only implementor that names real pinned checksums (module docs).
pub trait NodeBundleSpecs: Send + Sync {
    /// The bundle spec to ensure for `(version, variant)`.
    fn spec(&self, version: NodeVersion, variant: NodeVariant) -> RuntimeBundleSpec;
}

/// The real, pinned bundle specs (module docs' URLs/checksums) — the only
/// [`NodeBundleSpecs`] production code ever uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct PinnedNodeBundleSpecs;

impl NodeBundleSpecs for PinnedNodeBundleSpecs {
    fn spec(&self, version: NodeVersion, variant: NodeVariant) -> RuntimeBundleSpec {
        bundle_spec(version, variant)
    }
}

/// Returns the job's cached libc probe result, probing (and caching) once
/// on first use. Every action in a job runs in the *same* container, so the
/// probe genuinely only needs to happen once per job, not once per action.
///
/// # Errors
/// Returns [`RuntimeError`] only if the engine itself could not be reached.
pub(crate) async fn ensure_variant(
    engine: &dyn ContainerEngine,
    container: &str,
    cache: &mut Option<NodeVariant>,
) -> Result<NodeVariant, RuntimeError> {
    if let Some(variant) = cache {
        return Ok(*variant);
    }
    let variant = detect_node_variant(engine, container).await?;
    *cache = Some(variant);
    Ok(variant)
}

/// A sink that lowercases and accumulates stdout, for the libc probe's
/// tiny, line-oriented output.
#[derive(Default)]
struct CaptureLower {
    text: String,
}

impl ExecOutputSink for CaptureLower {
    fn on_stdout(&mut self, chunk: &[u8]) {
        self.text
            .push_str(&String::from_utf8_lossy(chunk).to_lowercase());
    }
    fn on_stderr(&mut self, _chunk: &[u8]) {}
}
