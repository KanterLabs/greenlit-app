//! The local artifact store behind `upload-artifact` / `download-artifact`.
//!
//! `PHASE-4-environment.md` ("Cache and artifacts"). The v4 actions reach
//! their service in two hops — a JSON twirp call to reserve or locate an
//! artifact, then a blob transfer to a URL that call returns — so the store
//! has to model both an artifact's *metadata* and the staged blocks its bytes
//! arrive in. The HTTP shapes live in [`crate::server`]; the block-list
//! ordering rule lives in [`blocklist`].
//!
//! # Scoping
//!
//! GitHub scopes an artifact to a workflow run, and the client identifies
//! that run with backend ids it derives from its own environment. Those ids
//! are treated here as **opaque strings**: the store never parses or
//! validates them, it only requires that the same id used to upload is used
//! to list or download. That keeps the store correct without depending on how
//! the client happens to derive them today.
//!
//! # On-disk layout
//!
//! ```text
//! <root>/staged/<id>/<block-id-hash>     one staged block, pre-commit
//! <root>/entries/<id>/meta.json          a finalized artifact
//! <root>/entries/<id>/blob               its assembled bytes
//! ```
//!
//! `<id>` is allocated by creating the directory, exactly as
//! [`crate::cache`] does, so two concurrent uploads cannot be handed the same
//! one. An artifact becomes visible to a list or download only when
//! `FinalizeArtifact` renames it into `entries/`, so an interrupted upload is
//! never downloadable as though it were whole.

pub mod blocklist;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// A finalized artifact's metadata document.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactMeta {
    /// The run this artifact belongs to, as the client identified it.
    scope: String,
    /// The artifact name, as authored in the workflow.
    name: String,
    /// Size in bytes, as the client's finalize call reported it.
    size: u64,
    /// Creation time, whole seconds since the Unix epoch.
    created_unix: u64,
}

/// One artifact, as a listing reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// The store's id for it, which doubles as its database id on the wire.
    pub id: i64,
    /// The run scope it belongs to.
    pub scope: String,
    /// Its name.
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// Creation time, whole seconds since the Unix epoch.
    pub created_unix: u64,
}

/// The local artifact store rooted at a directory.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// The default store root under a given home directory.
    #[must_use]
    pub fn default_path_under(home_dir: &Path) -> PathBuf {
        home_dir.join(".litci").join("artifacts")
    }

    /// Opens a store backed by an explicit root directory.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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

    fn staged_dir(&self) -> PathBuf {
        self.root.join("staged")
    }

    /// Reserves an id for a new artifact in `scope`, returning it.
    ///
    /// # Errors
    /// Returns [`StoreError::AlreadyReserved`] when `scope` already holds a
    /// finalized artifact of that name — GitHub refuses a duplicate name
    /// within one run — or [`StoreError::Io`] on a filesystem failure.
    pub fn create(&self, scope: &str, name: &str) -> Result<i64, StoreError> {
        if self
            .finalized()?
            .iter()
            .any(|(_, meta)| meta.scope == scope && meta.name == name)
        {
            return Err(StoreError::AlreadyReserved {
                key: name.to_string(),
            });
        }

        let staged = self.staged_dir();
        crate::layout::create_dir_all(&staged)?;
        let id = crate::layout::allocate_id(&staged, &self.entries_dir())?;
        let meta = ArtifactMeta {
            scope: scope.to_string(),
            name: name.to_string(),
            size: 0,
            created_unix: now_unix(),
        };
        write_meta(&staged.join(id.to_string()), &meta)?;
        Ok(id)
    }

    /// Stores one staged block for artifact `id`.
    ///
    /// Blocks arrive concurrently and under caller-chosen ids; nothing is
    /// assembled until the commit names their order.
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownReservation`] if `id` is not open, or
    /// [`StoreError::Io`] on a write failure.
    pub fn stage_block(&self, id: i64, block_id: &str, body: &[u8]) -> Result<(), StoreError> {
        let dir = self.staged_dir().join(id.to_string());
        if !dir.is_dir() {
            return Err(StoreError::UnknownReservation { id });
        }
        // A block id is a caller-chosen base64 string, so it is hashed rather
        // than used as a path component.
        let path = dir.join(format!("block-{}", crate::layout::name_hash(block_id)));
        fs::write(&path, body).map_err(|source| StoreError::Io {
            operation: "write the staged artifact block",
            path,
            source,
        })
    }

    /// Assembles artifact `id`'s blob from `block_ids`, in that order.
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownReservation`] if `id` is not open or a
    /// named block was never staged, or [`StoreError::Io`] on failure.
    pub fn commit_blocks(&self, id: i64, block_ids: &[String]) -> Result<(), StoreError> {
        let dir = self.staged_dir().join(id.to_string());
        if !dir.is_dir() {
            return Err(StoreError::UnknownReservation { id });
        }
        let mut assembled = Vec::new();
        for block_id in block_ids {
            let path = dir.join(format!("block-{}", crate::layout::name_hash(block_id)));
            let mut bytes = fs::read(&path).map_err(|source| StoreError::Io {
                operation: "read a staged artifact block",
                path,
                source,
            })?;
            assembled.append(&mut bytes);
        }
        let path = dir.join("blob");
        fs::write(&path, assembled).map_err(|source| StoreError::Io {
            operation: "assemble the artifact blob",
            path,
            source,
        })
    }

    /// Writes `body` as artifact `id`'s whole blob.
    ///
    /// The Azure client sends a single unstaged `PUT` when a payload is small
    /// enough not to need blocks, so the shim accepts that shape too rather
    /// than requiring a one-block staged upload.
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownReservation`] if `id` is not open, or
    /// [`StoreError::Io`] on a write failure.
    pub fn put_whole(&self, id: i64, body: &[u8]) -> Result<(), StoreError> {
        let dir = self.staged_dir().join(id.to_string());
        if !dir.is_dir() {
            return Err(StoreError::UnknownReservation { id });
        }
        let path = dir.join("blob");
        fs::write(&path, body).map_err(|source| StoreError::Io {
            operation: "write the artifact blob",
            path,
            source,
        })
    }

    /// Finalizes artifact `id`, making it listable and downloadable.
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownReservation`] if `id` is not open, or
    /// [`StoreError::Io`] on a filesystem failure.
    pub fn finalize(&self, id: i64, size: u64) -> Result<(), StoreError> {
        let from = self.staged_dir().join(id.to_string());
        if !from.is_dir() {
            return Err(StoreError::UnknownReservation { id });
        }
        let mut meta = read_meta(&from)?;
        meta.size = size;
        meta.created_unix = now_unix();
        write_meta(&from, &meta)?;

        let entries = self.entries_dir();
        crate::layout::create_dir_all(&entries)?;
        let to = entries.join(id.to_string());
        // Atomic within one filesystem: an artifact appears whole or not at
        // all, so a killed upload is never downloadable as if complete.
        fs::rename(&from, &to).map_err(|source| StoreError::Io {
            operation: "finalize the artifact",
            path: to,
            source,
        })
    }

    /// Every finalized artifact in `scope`, optionally filtered by name.
    ///
    /// # Errors
    /// Returns [`StoreError::Io`] if the entries directory cannot be read.
    pub fn list(&self, scope: &str, name: Option<&str>) -> Result<Vec<Artifact>, StoreError> {
        let mut found: Vec<Artifact> = self
            .finalized()?
            .into_iter()
            .filter(|(_, meta)| meta.scope == scope)
            .filter(|(_, meta)| name.is_none_or(|wanted| meta.name == wanted))
            .map(|(id, meta)| Artifact {
                id,
                scope: meta.scope,
                name: meta.name,
                size: meta.size,
                created_unix: meta.created_unix,
            })
            .collect();
        // A stable order keeps a listing reproducible across runs.
        found.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        Ok(found)
    }

    /// The path of a finalized artifact's blob.
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownReservation`] if no finalized artifact
    /// has this id.
    pub fn blob_path(&self, id: i64) -> Result<PathBuf, StoreError> {
        let path = self.entries_dir().join(id.to_string()).join("blob");
        if path.is_file() {
            Ok(path)
        } else {
            Err(StoreError::UnknownReservation { id })
        }
    }

    /// Removes a finalized artifact.
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownReservation`] if it does not exist.
    pub fn delete(&self, id: i64) -> Result<(), StoreError> {
        let dir = self.entries_dir().join(id.to_string());
        if !dir.is_dir() {
            return Err(StoreError::UnknownReservation { id });
        }
        fs::remove_dir_all(&dir).map_err(|source| StoreError::Io {
            operation: "remove the artifact",
            path: dir,
            source,
        })
    }

    /// The id of the still-staged artifact called `name` in `scope`.
    ///
    /// `FinalizeArtifact` names the artifact rather than echoing back the id
    /// its create call returned, so the upload in flight has to be recovered
    /// by name. Only one can exist at a time — [`ArtifactStore::create`]
    /// refuses a duplicate name in the same scope.
    #[must_use]
    pub fn pending(&self, scope: &str, name: &str) -> Option<i64> {
        let dir = self.staged_dir();
        let read = fs::read_dir(&dir).ok()?;
        read.flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let id = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.parse::<i64>().ok())?;
                let meta = read_meta(&path).ok()?;
                (meta.scope == scope && meta.name == name).then_some(id)
            })
            .next()
    }

    /// Every finalized artifact, as `(id, metadata)`.
    fn finalized(&self) -> Result<Vec<(i64, ArtifactMeta)>, StoreError> {
        let dir = self.entries_dir();
        let Ok(read) = fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut found = Vec::new();
        for entry in read {
            let entry = entry.map_err(|source| StoreError::Io {
                operation: "read the artifacts directory",
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
            // A torn metadata document means one unusable artifact, not a
            // broken store.
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

fn write_meta(dir: &Path, meta: &ArtifactMeta) -> Result<(), StoreError> {
    let path = dir.join("meta.json");
    let body = serde_json::to_vec(meta).map_err(|source| StoreError::CorruptMetadata {
        path: path.clone(),
        source,
    })?;
    fs::write(&path, body).map_err(|source| StoreError::Io {
        operation: "write the artifact metadata",
        path,
        source,
    })
}

fn read_meta(dir: &Path) -> Result<ArtifactMeta, StoreError> {
    let path = dir.join("meta.json");
    let body = fs::read(&path).map_err(|source| StoreError::Io {
        operation: "read the artifact metadata",
        path: path.clone(),
        source,
    })?;
    serde_json::from_slice(&body).map_err(|source| StoreError::CorruptMetadata { path, source })
}
