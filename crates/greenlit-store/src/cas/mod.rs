//! Machine-wide, digest-verified content-addressed storage.

mod catalog;
mod digest;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use catalog::Catalog;
pub use digest::{InvalidDigest, ObjectDigest};

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
                    writeln!(lock, "{}", std::process::id())
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
                    materialize(&partial, offset)?;
                    let (actual, size) = digest_file(&partial)?;
                    if &actual != digest {
                        self.quarantine(&partial, digest)?;
                        return Err(CasError::DigestMismatch {
                            expected: digest.clone(),
                            actual,
                        });
                    }
                    let outcome = self.publish_file(digest, &partial, size)?;
                    drop(guard);
                    return Ok(outcome);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
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
    let hash = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in hash {
        hex.push_str(&format!("{byte:02x}"));
    }
    ObjectDigest(format!("sha256:{hex}"))
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
