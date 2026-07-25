//! SQLite-WAL metadata catalog for immutable and leased local state.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use super::{CasError, ObjectDigest};

#[derive(Debug, Clone)]
pub(super) struct Catalog {
    path: PathBuf,
    connection: Arc<Mutex<Connection>>,
}

impl Catalog {
    pub(super) fn open(path: &Path) -> Result<Self, CasError> {
        let connection = open_connection(path)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS objects (
                digest TEXT PRIMARY KEY,
                size_bytes INTEGER NOT NULL,
                verified_at INTEGER NOT NULL,
                last_accessed INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS trees (
                digest TEXT PRIMARY KEY,
                manifest_digest TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS aliases (
                kind TEXT NOT NULL,
                requested TEXT NOT NULL,
                resolved TEXT NOT NULL,
                checked_at INTEGER NOT NULL,
                PRIMARY KEY(kind, requested)
             );
             CREATE TABLE IF NOT EXISTS object_refs (
                owner_kind TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                digest TEXT NOT NULL,
                PRIMARY KEY(owner_kind, owner_id, digest)
             );
             CREATE TABLE IF NOT EXISTS downloads (
                digest TEXT PRIMARY KEY,
                partial_bytes INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS leases (
                lease_id TEXT NOT NULL,
                digest TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                PRIMARY KEY(lease_id, digest)
             );
             CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY,
                lock_digest TEXT,
                state TEXT NOT NULL,
                updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS resources (
                resource_id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                state TEXT NOT NULL,
                updated_at INTEGER NOT NULL
             );",
            )
            .map_err(CasError::Catalog)?;
        Ok(Self {
            path: path.to_path_buf(),
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub(super) fn record_object(&self, digest: &ObjectDigest, size: u64) -> Result<(), CasError> {
        let size = i64::try_from(size).map_err(|_| CasError::ObjectTooLarge { size })?;
        let now = unix_seconds()?;
        self.connection()?
            .execute(
                "INSERT INTO objects(digest,size_bytes,verified_at,last_accessed)
                 VALUES(?1,?2,?3,?3)
                 ON CONFLICT(digest) DO UPDATE SET
                   size_bytes=excluded.size_bytes,
                   verified_at=excluded.verified_at,
                   last_accessed=excluded.last_accessed",
                params![digest.as_str(), size, now],
            )
            .map_err(CasError::Catalog)?;
        Ok(())
    }

    pub(super) fn record_download(
        &self,
        digest: &ObjectDigest,
        partial_bytes: u64,
    ) -> Result<(), CasError> {
        let partial_bytes = i64::try_from(partial_bytes).map_err(|_| CasError::ObjectTooLarge {
            size: partial_bytes,
        })?;
        let now = unix_seconds()?;
        self.connection()?
            .execute(
                "INSERT INTO downloads(digest,partial_bytes,updated_at)
                 VALUES(?1,?2,?3)
                 ON CONFLICT(digest) DO UPDATE SET
                   partial_bytes=excluded.partial_bytes,
                   updated_at=excluded.updated_at",
                params![digest.as_str(), partial_bytes, now],
            )
            .map_err(CasError::Catalog)?;
        Ok(())
    }

    pub(super) fn finish_download(&self, digest: &ObjectDigest) -> Result<(), CasError> {
        self.connection()?
            .execute(
                "DELETE FROM downloads WHERE digest=?1",
                params![digest.as_str()],
            )
            .map_err(CasError::Catalog)?;
        Ok(())
    }

    pub(super) fn record_tree(
        &self,
        digest: &ObjectDigest,
        manifest_digest: &ObjectDigest,
    ) -> Result<(), CasError> {
        self.connection()?
            .execute(
                "INSERT INTO trees(digest,manifest_digest) VALUES(?1,?2)
                 ON CONFLICT(digest) DO UPDATE SET manifest_digest=excluded.manifest_digest",
                params![digest.as_str(), manifest_digest.as_str()],
            )
            .map_err(CasError::Catalog)?;
        Ok(())
    }

    pub(super) fn record_alias(
        &self,
        kind: &str,
        requested: &str,
        resolved: &ObjectDigest,
    ) -> Result<(), CasError> {
        let now = unix_seconds()?;
        self.connection()?
            .execute(
                "INSERT INTO aliases(kind,requested,resolved,checked_at)
                 VALUES(?1,?2,?3,?4)
                 ON CONFLICT(kind,requested) DO UPDATE SET
                   resolved=excluded.resolved,
                   checked_at=excluded.checked_at",
                params![kind, requested, resolved.as_str(), now],
            )
            .map_err(CasError::Catalog)?;
        Ok(())
    }

    pub(super) fn resolve_alias(
        &self,
        kind: &str,
        requested: &str,
    ) -> Result<Option<ObjectDigest>, CasError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT resolved FROM aliases WHERE kind=?1 AND requested=?2")
            .map_err(CasError::Catalog)?;
        let mut rows = statement
            .query(params![kind, requested])
            .map_err(CasError::Catalog)?;
        let value: Option<String> = rows
            .next()
            .map_err(CasError::Catalog)?
            .map(|row| row.get(0))
            .transpose()
            .map_err(CasError::Catalog)?;
        value
            .map(|digest| {
                ObjectDigest::parse(&digest).map_err(|_| CasError::CatalogState {
                    path: self.path.display().to_string(),
                })
            })
            .transpose()
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, CasError> {
        self.connection.lock().map_err(|_| CasError::CatalogState {
            path: self.path.display().to_string(),
        })
    }
}

fn open_connection(path: &Path) -> Result<Connection, CasError> {
    let connection = Connection::open(path).map_err(CasError::Catalog)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(CasError::Catalog)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(CasError::Catalog)?;
    connection
        .busy_timeout(std::time::Duration::from_secs(10))
        .map_err(CasError::Catalog)?;
    Ok(connection)
}

fn unix_seconds() -> Result<i64, CasError> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(CasError::Clock)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| CasError::ClockRange)
}
