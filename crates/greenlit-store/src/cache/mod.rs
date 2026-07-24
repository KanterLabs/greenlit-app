//! The local `actions/cache` backing store.
//!
//! `PHASE-4-environment.md` ("Cache and artifacts"): the cache actions talk to
//! an HTTP API, so Greenlit serves that API locally and backs it with
//! `~/.litci/cache/`. This module owns the *store*; the HTTP shape lives in
//! [`crate::server`], and the selection rule in [`key`].
//!
//! # On-disk layout
//!
//! ```text
//! <root>/reservations/<id>/meta.json   an upload in flight
//! <root>/reservations/<id>/blob        its bytes, written at PATCH offsets
//! <root>/entries/<id>/meta.json        a committed entry
//! <root>/entries/<id>/blob             its bytes
//! ```
//!
//! `<id>` is the integer `cacheId` the client receives from a reservation and
//! sends back on every upload chunk and on commit, so no key text ever
//! becomes a path component. Ids come from [`crate::layout`], which allocates
//! them by creating the directory so two concurrent reservations cannot be
//! handed the same one even across processes.
//!
//! A committed entry is one that finished its `POST .../caches/<id>` commit.
//! An interrupted upload leaves a `reservations/<id>/` directory that no
//! lookup can ever see, because lookups read `entries/` only — the same
//! "a killed fetch can never look like a hit" property `greenlit-actions`'
//! action store has.

pub mod key;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::StoreError;

pub use key::{Candidate, Match};

/// A committed entry's metadata document (`meta.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EntryMeta {
    /// The key the entry was saved under.
    key: String,
    /// The opaque version (a hash of the action's resolved paths and
    /// compression method) the entry was saved under.
    version: String,
    /// Committed size in bytes, as reported by the client's commit request.
    size: u64,
    /// Creation time, whole seconds since the Unix epoch.
    created_unix: u64,
}

/// A successful cache lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// The id of the entry that matched.
    pub id: i64,
    /// The key that actually matched, which the client compares against its
    /// primary key to decide `cache-hit`.
    pub key: String,
    /// Whether this was an exact match on the primary key.
    pub exact: bool,
}

/// A running tally of lookups, for the end-of-run breakdown
/// `PHASE-4-environment.md` asks for ("Instrument cache-shim hit/miss").
///
/// Every clone of a [`CacheStore`] shares one tally, which is what lets the
/// host process read counts accumulated by the shim's own task: the shim
/// serves on a `tokio::spawn`ed task that does not inherit the scoped tracing
/// subscriber `greenlit-metrics` installs, so a span opened inside a handler
/// would never be recorded. Counters are subscriber-independent and work
/// regardless of which task touched the store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheCounts {
    /// Lookups that restored an entry.
    pub hits: u64,
    /// Lookups that found nothing.
    pub misses: u64,
    /// Bytes committed into the store by save operations.
    pub bytes_written: u64,
}

#[derive(Debug, Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    bytes_written: AtomicU64,
}

/// The local cache store rooted at a directory.
#[derive(Debug, Clone)]
pub struct CacheStore {
    root: PathBuf,
    counters: Arc<Counters>,
}

impl CacheStore {
    /// The default store root under a given home directory:
    /// `<home>/.litci/cache` (`AGENTS.md`: "User-local state | `~/.litci/`"),
    /// matching `greenlit_actions::ActionStore::default_path_under`'s pure,
    /// side-effect-free shape.
    #[must_use]
    pub fn default_path_under(home_dir: &Path) -> PathBuf {
        home_dir.join(".litci").join("cache")
    }

    /// Opens a store backed by an explicit root directory.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            counters: Arc::new(Counters::default()),
        }
    }

    /// The lookup tally across this store and every clone of it.
    #[must_use]
    pub fn counts(&self) -> CacheCounts {
        CacheCounts {
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
            bytes_written: self.counters.bytes_written.load(Ordering::Relaxed),
        }
    }

    /// Opens the real per-user store, resolving `HOME`.
    ///
    /// # Errors
    /// Returns [`StoreError::HomeDirUnavailable`] or
    /// [`StoreError::InvalidHomeDir`] when `HOME` is unset or relative.
    pub fn open_default() -> Result<Self, StoreError> {
        let home = std::env::var_os("HOME").ok_or(StoreError::HomeDirUnavailable)?;
        let home = Path::new(&home);
        if !home.is_absolute() {
            return Err(StoreError::InvalidHomeDir);
        }
        Ok(Self::at(Self::default_path_under(home)))
    }

    /// The store's root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entries_dir(&self) -> PathBuf {
        self.root.join("entries")
    }

    fn reservations_dir(&self) -> PathBuf {
        self.root.join("reservations")
    }

    /// Selects the entry a lookup restores, or `None` on a miss.
    ///
    /// `keys` is the ordered `[key, ...restore_keys]` list exactly as
    /// `actions/cache` sends it; see [`key`] for the rule.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the entries directory exists but cannot
    /// be read. A missing entries directory is an empty store, not an error.
    pub fn lookup(&self, keys: &[String], version: &str) -> Result<Option<Restored>, StoreError> {
        let committed = self.committed()?;
        let candidates: Vec<Candidate> = committed
            .iter()
            .map(|(_, meta)| Candidate {
                key: meta.key.clone(),
                version: meta.version.clone(),
                created_unix: meta.created_unix,
            })
            .collect();

        let Some(selected) = key::select(keys, version, candidates.iter()) else {
            self.counters.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        };
        self.counters.hits.fetch_add(1, Ordering::Relaxed);

        // `select` returns the winning *key*; map it back to the id whose
        // metadata produced that candidate. Keys are unique per version, and
        // the version was already matched, so this identifies one entry.
        let id = committed
            .iter()
            .find(|(_, meta)| meta.key == selected.key && meta.version == version)
            .map(|(id, _)| *id);

        Ok(id.map(|id| Restored {
            id,
            key: selected.key,
            exact: selected.exact,
        }))
    }

    /// Reserves an id for a new entry, returning the `cacheId` the client
    /// uploads against.
    ///
    /// # Errors
    /// Returns [`StoreError::AlreadyReserved`] when `key` is already
    /// committed at this `version` — the hosted service answers that case
    /// with HTTP 409, which `actions/cache` treats as a benign "another job
    /// saved it first". Returns [`StoreError::Io`] on a filesystem failure.
    pub fn reserve(&self, key: &str, version: &str) -> Result<i64, StoreError> {
        if self
            .committed()?
            .iter()
            .any(|(_, meta)| meta.key == key && meta.version == version)
        {
            return Err(StoreError::AlreadyReserved {
                key: key.to_string(),
            });
        }

        let reservations = self.reservations_dir();
        crate::layout::create_dir_all(&reservations)?;
        let id = crate::layout::allocate_id(&reservations, &self.entries_dir())?;

        let meta = EntryMeta {
            key: key.to_string(),
            version: version.to_string(),
            size: 0,
            created_unix: now_unix(),
        };
        write_meta(&reservations.join(id.to_string()), &meta)?;
        Ok(id)
    }

    /// Writes `chunk` at `offset` into reservation `id`'s blob.
    ///
    /// `actions/cache` uploads concurrently at explicit `Content-Range`
    /// offsets, so writes are positional rather than appended.
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownReservation`] if `id` is not open, or
    /// [`StoreError::Io`] on a write failure.
    pub fn write_chunk(&self, id: i64, offset: u64, chunk: &[u8]) -> Result<(), StoreError> {
        let dir = self.reservations_dir().join(id.to_string());
        if !dir.is_dir() {
            return Err(StoreError::UnknownReservation { id });
        }
        write_at(&dir.join("blob"), offset, chunk)
    }

    /// Commits reservation `id`, recording `size` and making the entry
    /// visible to [`CacheStore::lookup`].
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownReservation`] if `id` is not open, or
    /// [`StoreError::Io`] on a filesystem failure.
    pub fn commit(&self, id: i64, size: u64) -> Result<(), StoreError> {
        let from = self.reservations_dir().join(id.to_string());
        if !from.is_dir() {
            return Err(StoreError::UnknownReservation { id });
        }

        self.counters
            .bytes_written
            .fetch_add(size, Ordering::Relaxed);
        let mut meta = read_meta(&from)?;
        meta.size = size;
        meta.created_unix = now_unix();
        write_meta(&from, &meta)?;

        let entries = self.entries_dir();
        crate::layout::create_dir_all(&entries)?;
        let to = entries.join(id.to_string());
        // A rename is atomic within one filesystem, so an entry becomes
        // visible whole or not at all -- there is no window where `lookup`
        // can see a directory whose blob is still being written.
        fs::rename(&from, &to).map_err(|source| StoreError::Io {
            operation: "commit the cache entry",
            path: to,
            source,
        })
    }

    /// The path of a committed entry's blob.
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownReservation`] if no committed entry has
    /// this id.
    pub fn blob_path(&self, id: i64) -> Result<PathBuf, StoreError> {
        let path = self.entries_dir().join(id.to_string()).join("blob");
        if path.is_file() {
            Ok(path)
        } else {
            Err(StoreError::UnknownReservation { id })
        }
    }

    /// Every committed entry, as `(id, metadata)`.
    ///
    /// An entry whose `meta.json` is missing or unparseable is skipped rather
    /// than failing the caller: a torn document written by a killed run means
    /// one unusable entry, not a broken store.
    fn committed(&self) -> Result<Vec<(i64, EntryMeta)>, StoreError> {
        let dir = self.entries_dir();
        let Ok(read) = fs::read_dir(&dir) else {
            // A store that has never committed anything has no directory yet.
            return Ok(Vec::new());
        };

        let mut found = Vec::new();
        for entry in read {
            let entry = entry.map_err(|source| StoreError::Io {
                operation: "read the cache entries directory",
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<i64>().ok())
            else {
                continue;
            };
            if let Ok(meta) = read_meta(&path) {
                found.push((id, meta));
            }
        }
        Ok(found)
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn write_meta(dir: &Path, meta: &EntryMeta) -> Result<(), StoreError> {
    let path = dir.join("meta.json");
    let body = serde_json::to_vec(meta).map_err(|source| StoreError::CorruptMetadata {
        path: path.clone(),
        source,
    })?;
    fs::write(&path, body).map_err(|source| StoreError::Io {
        operation: "write the entry metadata",
        path,
        source,
    })
}

fn read_meta(dir: &Path) -> Result<EntryMeta, StoreError> {
    let path = dir.join("meta.json");
    let body = fs::read(&path).map_err(|source| StoreError::Io {
        operation: "read the entry metadata",
        path: path.clone(),
        source,
    })?;
    serde_json::from_slice(&body).map_err(|source| StoreError::CorruptMetadata { path, source })
}

/// Writes `chunk` at `offset`, extending the file with zeroes if the upload
/// delivered a later range first.
fn write_at(path: &Path, offset: u64, chunk: &[u8]) -> Result<(), StoreError> {
    use std::io::{Seek, SeekFrom, Write};

    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|source| StoreError::Io {
            operation: "open the cache blob for writing",
            path: path.to_path_buf(),
            source,
        })?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|source| StoreError::Io {
            operation: "seek the cache blob",
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(chunk).map_err(|source| StoreError::Io {
        operation: "write the cache blob",
        path: path.to_path_buf(),
        source,
    })
}
