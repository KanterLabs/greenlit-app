//! Machine-wide, digest-verified content-addressed storage.

mod catalog;
mod digest;
mod http;
mod tree;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use catalog::Catalog;
pub use digest::{InvalidDigest, ObjectDigest};
pub use http::HttpFetch;
pub use tree::{TreeEntry, TreeEntryKind, TreeManifest};

const LOCK_WAIT: Duration = Duration::from_secs(120);
const LOCK_POLL: Duration = Duration::from_millis(25);

/// Whether ensuring an object reused or published content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// A verified object already existed.
    Hit,
    /// This caller materialized and atomically published the object.
    Published,
    /// Another process published the object while this caller waited.
    Shared,
}

/// Read-only storage health and reclaimability report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreDoctorReport {
    /// Metadata/filesystem inconsistencies that block destructive GC.
    pub issues: Vec<String>,
    /// Distinct unexpired run leases.
    pub active_leases: u64,
    /// Immutable objects eligible for collection.
    pub reclaimable_objects: usize,
    /// Bytes in eligible immutable objects.
    pub reclaimable_bytes: u64,
    /// Retained interrupted partial downloads, reclaimed before objects.
    pub partial_downloads: usize,
    /// Bytes in retained interrupted partial downloads.
    pub partial_bytes: u64,
}

impl StoreDoctorReport {
    /// Whether destructive collection is safe.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Result of one reference-aware collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GarbageCollection {
    /// Interrupted partial downloads removed first.
    pub partial_downloads: usize,
    /// Unreferenced and unleased immutable objects removed.
    pub objects: usize,
    /// Total filesystem bytes reclaimed.
    pub bytes: u64,
}

/// Heartbeating lease for immutable objects used by one active run.
pub struct LeaseGuard {
    lease_id: String,
    store: CasStore,
    stop: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    heartbeat: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for LeaseGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LeaseGuard")
            .field("lease_id", &self.lease_id)
            .finish_non_exhaustive()
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let (lock, wake) = &*self.stop;
        if let Ok(mut stopped) = lock.lock() {
            *stopped = true;
            wake.notify_all();
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _result = heartbeat.join();
        }
        let _result = self.store.release_lease(&self.lease_id);
    }
}

/// A content-store failure.
#[derive(Debug, thiserror::Error)]
pub enum CasError {
    /// Filesystem operation failed.
    #[error("content store path {path}: {source}")]
    Io {
        /// Affected path.
        path: String,
        /// I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// SQLite catalog operation failed.
    #[error("content catalog: {0}")]
    Catalog(#[source] rusqlite::Error),
    /// The in-process catalog lock is unavailable.
    #[error("content catalog state at {path} is unavailable")]
    CatalogState {
        /// Catalog path.
        path: String,
    },
    /// The supplied bytes did not match the requested identity.
    #[error("downloaded content does not match {expected}; computed {actual}")]
    DigestMismatch {
        /// Required identity.
        expected: ObjectDigest,
        /// Actual identity.
        actual: ObjectDigest,
    },
    /// Another process did not finish the same object in time.
    #[error("timed out waiting for the in-flight download of {digest}")]
    InFlightTimeout {
        /// Requested object.
        digest: ObjectDigest,
    },
    /// System clock is invalid.
    #[error("system clock is before the Unix epoch: {0}")]
    Clock(#[source] std::time::SystemTimeError),
    /// System clock cannot fit the catalog schema.
    #[error("system clock value exceeds the content catalog range")]
    ClockRange,
    /// Object size cannot fit the catalog schema.
    #[error("object size {size} exceeds the content catalog range")]
    ObjectTooLarge {
        /// Rejected byte length.
        size: u64,
    },
    /// Offline mode was requested but the exact object is absent.
    #[error("offline content is missing: {digest} ({source_url})")]
    OfflineMissing {
        /// Required immutable identity.
        digest: ObjectDigest,
        /// Locked source that would provide it online.
        source_url: String,
    },
    /// An immutable HTTP download failed.
    #[error("could not fetch {digest} from {source_url}: {message}")]
    Http {
        /// Required immutable identity.
        digest: ObjectDigest,
        /// Locked source URL.
        source_url: String,
        /// Bounded transport or protocol detail.
        message: String,
    },
    /// A server returned a resume range different from the requested offset.
    #[error(
        "could not resume {digest} from {source_url}: requested byte {requested}, server returned {returned}"
    )]
    ResumeMismatch {
        /// Required immutable identity.
        digest: ObjectDigest,
        /// Locked source URL.
        source_url: String,
        /// Requested first byte.
        requested: u64,
        /// Returned Content-Range value.
        returned: String,
    },
    /// A response exceeded the caller's immutable-object bound.
    #[error("content for {digest} from {source_url} exceeds the {limit}-byte limit")]
    ResponseTooLarge {
        /// Required immutable identity.
        digest: ObjectDigest,
        /// Locked source URL.
        source_url: String,
        /// Configured byte limit.
        limit: u64,
    },
    /// Canonical tree metadata could not be encoded or decoded.
    #[error("content tree metadata: {0}")]
    TreeJson(#[source] serde_json::Error),
    /// A tree contains a path or node type that cannot be materialized safely.
    #[error("invalid content tree entry {path}: {reason}")]
    InvalidTree {
        /// Relative tree path.
        path: String,
        /// Rejection reason.
        reason: String,
    },
}

/// A machine-wide SHA-256 object store with SQLite-WAL metadata.
#[derive(Debug, Clone)]
pub struct CasStore {
    root: PathBuf,
    catalog: Catalog,
}

impl CasStore {
    /// Opens or initializes a store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CasError> {
        let root = root.into();
        for child in ["objects/sha256", "tmp", "inflight", "quarantine"] {
            let path = root.join(child);
            fs::create_dir_all(&path).map_err(|source| io_error(&path, source))?;
        }
        let catalog = Catalog::open(&root.join("catalog.sqlite3"))?;
        Ok(Self { root, catalog })
    }

    /// Returns the default machine-user store path.
    #[must_use]
    pub fn default_path_under(home: &Path) -> PathBuf {
        home.join(".litci").join("store")
    }

    /// Reads and verifies an object. Corruption is quarantined and returned
    /// as a digest mismatch, never as usable bytes.
    pub fn read_verified(&self, digest: &ObjectDigest) -> Result<Option<Vec<u8>>, CasError> {
        let path = self.object_path(digest);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error(&path, source)),
        };
        let actual = digest_bytes(&bytes);
        if &actual != digest {
            self.quarantine(&path, digest)?;
            return Err(CasError::DigestMismatch {
                expected: digest.clone(),
                actual,
            });
        }
        Ok(Some(bytes))
    }

    /// Atomically publishes bytes only when they match `digest`.
    pub fn put_verified(
        &self,
        digest: &ObjectDigest,
        bytes: &[u8],
    ) -> Result<EnsureOutcome, CasError> {
        if self.usable_or_quarantined(digest)? {
            return Ok(EnsureOutcome::Hit);
        }
        let actual = digest_bytes(bytes);
        if &actual != digest {
            return Err(CasError::DigestMismatch {
                expected: digest.clone(),
                actual,
            });
        }
        self.publish_bytes(digest, bytes)
    }

    /// Verifies and publishes a file without buffering the object in memory.
    pub fn put_file_verified(
        &self,
        digest: &ObjectDigest,
        source: &Path,
    ) -> Result<EnsureOutcome, CasError> {
        if self.usable_or_quarantined(digest)? {
            return Ok(EnsureOutcome::Hit);
        }
        self.ensure_with(digest, |partial, _offset| {
            fs::copy(source, partial)
                .map(|_| ())
                .map_err(|error| io_error(source, error))
        })
    }

    /// Runs `materialize` at most once across cooperating processes for a
    /// missing digest. The callback receives a persistent partial path and
    /// its current byte length, enabling HTTP Range resumption.
    pub fn ensure_with(
        &self,
        digest: &ObjectDigest,
        materialize: impl FnOnce(&Path, u64) -> Result<(), CasError>,
    ) -> Result<EnsureOutcome, CasError> {
        if self.usable_or_quarantined(digest)? {
            return Ok(EnsureOutcome::Hit);
        }
        let lock_path = self.root.join("inflight").join(digest.hex());
        let start = Instant::now();
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut lock) => {
                    writeln!(lock, "{}", current_process_identity())
                        .map_err(|source| io_error(&lock_path, source))?;
                    lock.sync_all()
                        .map_err(|source| io_error(&lock_path, source))?;
                    let guard = InFlightGuard { path: lock_path };
                    if self.usable_or_quarantined(digest)? {
                        return Ok(EnsureOutcome::Shared);
                    }
                    let partial = self
                        .root
                        .join("tmp")
                        .join(format!("{}.partial", digest.hex()));
                    let offset = fs::metadata(&partial).map_or(0, |metadata| metadata.len());
                    self.catalog.record_download(digest, offset)?;
                    if let Err(error) = materialize(&partial, offset) {
                        let retained =
                            fs::metadata(&partial).map_or(offset, |metadata| metadata.len());
                        self.catalog.record_download(digest, retained)?;
                        return Err(error);
                    }
                    let retained = fs::metadata(&partial).map_or(offset, |metadata| metadata.len());
                    self.catalog.record_download(digest, retained)?;
                    let (actual, size) = digest_file(&partial)?;
                    if &actual != digest {
                        self.quarantine(&partial, digest)?;
                        return Err(CasError::DigestMismatch {
                            expected: digest.clone(),
                            actual,
                        });
                    }
                    let outcome = self.publish_file(digest, &partial, size)?;
                    self.catalog.finish_download(digest)?;
                    drop(guard);
                    return Ok(outcome);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if stale_lock(&lock_path)? {
                        match fs::remove_file(&lock_path) {
                            Ok(()) => continue,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(source) => return Err(io_error(&lock_path, source)),
                        }
                    }
                    if self.usable_or_quarantined(digest)? {
                        return Ok(EnsureOutcome::Shared);
                    }
                    if start.elapsed() >= LOCK_WAIT {
                        return Err(CasError::InFlightTimeout {
                            digest: digest.clone(),
                        });
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                Err(source) => return Err(io_error(&lock_path, source)),
            }
        }
    }

    /// Ensures an exact object from HTTP, resuming a retained partial with a
    /// `Range` request and refusing any network access in offline mode.
    ///
    /// Servers that ignore a valid Range request and return the entire object
    /// with status 200 are handled by restarting the partial from byte zero.
    /// A 206 response must begin at the requested byte.
    pub fn ensure_http(
        &self,
        digest: &ObjectDigest,
        fetch: &HttpFetch,
    ) -> Result<EnsureOutcome, CasError> {
        if self.usable_or_quarantined(digest)? {
            return Ok(EnsureOutcome::Hit);
        }
        if fetch.offline {
            return Err(CasError::OfflineMissing {
                digest: digest.clone(),
                source_url: fetch.url.clone(),
            });
        }
        self.ensure_with(digest, |partial, offset| {
            http::download(partial, offset, digest, fetch)
        })
    }

    /// Returns the verified object's filesystem path when it exists.
    ///
    /// Corrupt content is quarantined and reported instead of returned.
    pub fn verified_path(&self, digest: &ObjectDigest) -> Result<Option<PathBuf>, CasError> {
        if self.has_verified(digest)? {
            Ok(Some(self.object_path(digest)))
        } else {
            Ok(None)
        }
    }

    /// Records a stable alias to an immutable object or tree identity.
    pub fn record_alias(
        &self,
        kind: &str,
        requested: &str,
        resolved: &ObjectDigest,
    ) -> Result<(), CasError> {
        self.catalog.record_alias(kind, requested, resolved)
    }

    /// Resolves an alias previously recorded by [`Self::record_alias`].
    pub fn resolve_alias(
        &self,
        kind: &str,
        requested: &str,
    ) -> Result<Option<ObjectDigest>, CasError> {
        self.catalog.resolve_alias(kind, requested)
    }

    /// Records a metadata alias whose resolved value is not a SHA-256 object,
    /// such as a Git commit identity.
    pub fn record_text_alias(
        &self,
        kind: &str,
        requested: &str,
        resolved: &str,
    ) -> Result<(), CasError> {
        self.catalog.record_text_alias(kind, requested, resolved)
    }

    /// Resolves a metadata alias recorded by [`Self::record_text_alias`].
    pub fn resolve_text_alias(
        &self,
        kind: &str,
        requested: &str,
    ) -> Result<Option<String>, CasError> {
        self.catalog.resolve_text_alias(kind, requested)
    }

    /// Acquire or replace a digest lease for an active run.
    pub fn acquire_lease(
        &self,
        lease_id: &str,
        digests: &[ObjectDigest],
        ttl: Duration,
    ) -> Result<(), CasError> {
        self.catalog
            .acquire_lease(lease_id, digests, expiry_after(ttl)?)
    }

    /// Acquire an active-run lease and heartbeat it until the guard drops.
    pub fn lease_guard(
        &self,
        lease_id: impl Into<String>,
        digests: &[ObjectDigest],
    ) -> Result<LeaseGuard, CasError> {
        const TTL: Duration = Duration::from_secs(60);
        const HEARTBEAT: Duration = Duration::from_secs(10);
        let lease_id = lease_id.into();
        self.acquire_lease(&lease_id, digests, TTL)?;
        let store = self.clone();
        let heartbeat_id = lease_id.clone();
        let stop = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let thread_stop = stop.clone();
        let heartbeat = std::thread::Builder::new()
            .name("greenlit-lease".to_string())
            .spawn(move || {
                let (lock, wake) = &*thread_stop;
                let Ok(mut stopped) = lock.lock() else {
                    return;
                };
                loop {
                    let Ok((next, timeout)) = wake.wait_timeout(stopped, HEARTBEAT) else {
                        return;
                    };
                    stopped = next;
                    if *stopped {
                        return;
                    }
                    if timeout.timed_out() {
                        let _result = store.heartbeat_lease(&heartbeat_id, TTL);
                    }
                }
            })
            .map_err(|source| CasError::Io {
                path: self.root.display().to_string(),
                source,
            })?;
        Ok(LeaseGuard {
            lease_id,
            store: self.clone(),
            stop,
            heartbeat: Some(heartbeat),
        })
    }

    /// Extend every digest held by one active run lease.
    pub fn heartbeat_lease(&self, lease_id: &str, ttl: Duration) -> Result<usize, CasError> {
        self.catalog.heartbeat_lease(lease_id, expiry_after(ttl)?)
    }

    /// Release every digest held by one run.
    pub fn release_lease(&self, lease_id: &str) -> Result<(), CasError> {
        self.catalog.release_lease(lease_id)
    }

    /// Whether one run currently holds at least one unexpired digest lease.
    pub fn lease_is_active(&self, lease_id: &str) -> Result<bool, CasError> {
        self.catalog.lease_is_active(lease_id, unix_seconds()?)
    }

    /// Keep immutable objects referenced by a retained RunLock or user pin.
    pub fn pin_objects(
        &self,
        owner_kind: &str,
        owner_id: &str,
        digests: &[ObjectDigest],
    ) -> Result<(), CasError> {
        self.catalog.pin_objects(owner_kind, owner_id, digests)
    }

    /// Persist a durable run state transition.
    pub fn record_run_state(
        &self,
        run_id: &str,
        lock_digest: Option<&str>,
        state: &str,
    ) -> Result<(), CasError> {
        self.catalog.record_run_state(run_id, lock_digest, state)
    }

    /// Inspect catalog consistency, active leases, and reclaimable bytes
    /// without deleting anything.
    pub fn doctor(&self) -> Result<StoreDoctorReport, CasError> {
        let now = unix_seconds()?;
        let mut issues = self.catalog.integrity_issues()?;
        for (digest, expected_size) in self.catalog.all_objects()? {
            let path = self.object_path(&digest);
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() && metadata.len() == expected_size => {}
                Ok(metadata) => issues.push(format!(
                    "catalog object {digest} expects {expected_size} bytes but {} has {} bytes",
                    path.display(),
                    metadata.len()
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => issues.push(format!(
                    "catalog object {digest} is missing from {}",
                    path.display()
                )),
                Err(error) => issues.push(format!(
                    "catalog object {digest} could not be inspected at {}: {error}",
                    path.display()
                )),
            }
        }
        let reclaimable = self.catalog.reclaimable_objects(now)?;
        let (partial_downloads, partial_bytes) = directory_files(&self.root.join("tmp"))?;
        Ok(StoreDoctorReport {
            issues,
            active_leases: self.catalog.active_lease_count(now)?,
            reclaimable_objects: reclaimable.len(),
            reclaimable_bytes: reclaimable.iter().map(|(_, size)| size).sum(),
            partial_downloads,
            partial_bytes,
        })
    }

    /// Remove interrupted partials first, then unreferenced immutable objects.
    ///
    /// Any metadata inconsistency refuses the entire destructive operation.
    pub fn collect_garbage(&self) -> Result<GarbageCollection, CasError> {
        let report = self.doctor()?;
        if !report.is_consistent() {
            return Err(CasError::CatalogState {
                path: self.root.join("catalog.sqlite3").display().to_string(),
            });
        }
        let mut collection = GarbageCollection {
            partial_downloads: 0,
            objects: 0,
            bytes: 0,
        };
        let tmp = self.root.join("tmp");
        for entry in fs::read_dir(&tmp).map_err(|error| io_error(&tmp, error))? {
            let entry = entry.map_err(|error| io_error(&tmp, error))?;
            let path = entry.path();
            let metadata = entry.metadata().map_err(|error| io_error(&path, error))?;
            if metadata.is_file() {
                fs::remove_file(&path).map_err(|error| io_error(&path, error))?;
                collection.partial_downloads = collection.partial_downloads.saturating_add(1);
                collection.bytes = collection.bytes.saturating_add(metadata.len());
            }
        }
        for (digest, size) in self.catalog.reclaimable_objects(unix_seconds()?)? {
            let path = self.object_path(&digest);
            fs::remove_file(&path).map_err(|error| io_error(&path, error))?;
            self.catalog.remove_object(&digest)?;
            collection.objects = collection.objects.saturating_add(1);
            collection.bytes = collection.bytes.saturating_add(size);
        }
        Ok(collection)
    }

    /// Ingests a directory as a canonical tree whose file and symlink
    /// payloads are separate verified CAS objects.
    pub fn put_tree(&self, root: &Path) -> Result<ObjectDigest, CasError> {
        tree::put(self, root)
    }

    /// Computes a canonical tree identity without publishing or consulting
    /// payload objects. This is the fast integrity check for a previously
    /// migrated legacy tree.
    pub fn tree_digest(&self, root: &Path) -> Result<ObjectDigest, CasError> {
        tree::digest(root)
    }

    /// Materializes an ingested tree into an existing empty directory.
    pub fn materialize_tree(
        &self,
        digest: &ObjectDigest,
        destination: &Path,
    ) -> Result<(), CasError> {
        tree::materialize(self, digest, destination)
    }

    fn publish_bytes(
        &self,
        digest: &ObjectDigest,
        bytes: &[u8],
    ) -> Result<EnsureOutcome, CasError> {
        let target = self.object_path(digest);
        let parent = target.parent().ok_or_else(|| {
            io_error(
                &target,
                std::io::Error::other("object path has no parent directory"),
            )
        })?;
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        let temp =
            self.root
                .join("tmp")
                .join(format!("{}.{}.publish", digest.hex(), std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|source| io_error(&temp, source))?;
        file.write_all(bytes)
            .map_err(|source| io_error(&temp, source))?;
        file.sync_all().map_err(|source| io_error(&temp, source))?;
        match fs::rename(&temp, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temp).map_err(|source| io_error(&temp, source))?;
                if self.read_verified(digest)?.is_some() {
                    return Ok(EnsureOutcome::Shared);
                }
            }
            Err(source) => return Err(io_error(&target, source)),
        }
        sync_directory(parent)?;
        self.catalog.record_object(digest, bytes.len() as u64)?;
        Ok(EnsureOutcome::Published)
    }

    fn has_verified(&self, digest: &ObjectDigest) -> Result<bool, CasError> {
        let path = self.object_path(digest);
        let (actual, _size) = match digest_file(&path) {
            Ok(value) => value,
            Err(CasError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        if &actual != digest {
            self.quarantine(&path, digest)?;
            return Err(CasError::DigestMismatch {
                expected: digest.clone(),
                actual,
            });
        }
        Ok(true)
    }

    fn usable_or_quarantined(&self, digest: &ObjectDigest) -> Result<bool, CasError> {
        match self.has_verified(digest) {
            Ok(present) => Ok(present),
            Err(CasError::DigestMismatch { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn publish_file(
        &self,
        digest: &ObjectDigest,
        source: &Path,
        size: u64,
    ) -> Result<EnsureOutcome, CasError> {
        let target = self.object_path(digest);
        let parent = target.parent().ok_or_else(|| {
            io_error(
                &target,
                std::io::Error::other("object path has no parent directory"),
            )
        })?;
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        File::open(source)
            .and_then(|file| file.sync_all())
            .map_err(|error| io_error(source, error))?;
        fs::rename(source, &target).map_err(|error| io_error(&target, error))?;
        sync_directory(parent)?;
        self.catalog.record_object(digest, size)?;
        Ok(EnsureOutcome::Published)
    }

    fn object_path(&self, digest: &ObjectDigest) -> PathBuf {
        let hex = digest.hex();
        self.root
            .join("objects")
            .join("sha256")
            .join(&hex[..2])
            .join(&hex[2..])
    }

    fn quarantine(&self, path: &Path, digest: &ObjectDigest) -> Result<(), CasError> {
        let destination =
            self.root
                .join("quarantine")
                .join(format!("{}.{}", digest.hex(), std::process::id()));
        match fs::rename(path, &destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io_error(path, source)),
        }
    }
}

struct InFlightGuard {
    path: PathBuf,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let _result = fs::remove_file(&self.path);
    }
}

fn digest_bytes(bytes: &[u8]) -> ObjectDigest {
    ObjectDigest::of_bytes(bytes)
}

fn expiry_after(ttl: Duration) -> Result<i64, CasError> {
    let now = unix_seconds()?;
    let seconds = i64::try_from(ttl.as_secs()).map_err(|_| CasError::ClockRange)?;
    now.checked_add(seconds).ok_or(CasError::ClockRange)
}

fn unix_seconds() -> Result<i64, CasError> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(CasError::Clock)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| CasError::ClockRange)
}

fn directory_files(path: &Path) -> Result<(usize, u64), CasError> {
    let mut count = 0_usize;
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path).map_err(|error| io_error(path, error))? {
        let entry = entry.map_err(|error| io_error(path, error))?;
        let metadata = entry
            .metadata()
            .map_err(|error| io_error(&entry.path(), error))?;
        if metadata.is_file() {
            count = count.saturating_add(1);
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok((count, bytes))
}

fn digest_file(path: &Path) -> Result<(ObjectDigest, u64), CasError> {
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_error(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok((ObjectDigest(format!("sha256:{hex}")), size))
}

fn sync_directory(path: &Path) -> Result<(), CasError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: std::io::Error) -> CasError {
    CasError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn current_process_identity() -> String {
    let pid = std::process::id();
    process_identity(pid).unwrap_or_else(|| format!("{pid} unknown unknown"))
}

fn process_identity(pid: u32) -> Option<String> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()?
        .trim()
        .to_string();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(") ")?.1;
    let start_ticks = after_name.split_whitespace().nth(19)?;
    Some(format!("{pid} {boot} {start_ticks}"))
}

fn stale_lock(path: &Path) -> Result<bool, CasError> {
    let owner = match fs::read_to_string(path) {
        Ok(owner) => owner.trim().to_string(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(source) => return Err(io_error(path, source)),
    };
    let mut fields = owner.split_whitespace();
    let Some(pid) = fields.next().and_then(|field| field.parse::<u32>().ok()) else {
        return lock_old_enough(path, Duration::from_secs(2));
    };
    if fields.next().is_none() || fields.next().is_none() || fields.next().is_some() {
        return lock_old_enough(path, Duration::from_secs(2));
    }
    Ok(process_identity(pid).is_none_or(|identity| identity != owner))
}

fn lock_old_enough(path: &Path, minimum_age: Duration) -> Result<bool, CasError> {
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|source| io_error(path, source))?;
    Ok(modified.elapsed().is_ok_and(|age| age >= minimum_age))
}
