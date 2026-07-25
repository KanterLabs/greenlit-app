//! Canonical immutable directory trees.

use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{CasError, CasStore, ObjectDigest};

const MAX_TREE_ENTRIES: usize = 200_000;

/// One canonical tree manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeManifest {
    /// Schema version for stable decoding.
    pub schema_version: u32,
    /// Entries ordered by raw relative-path bytes.
    pub entries: Vec<TreeEntry>,
}

/// One directory-tree entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    /// Slash-separated relative path encoded losslessly as hexadecimal bytes.
    pub path_hex: String,
    /// Filesystem node kind.
    pub kind: TreeEntryKind,
    /// Permission bits for regular files and directories.
    pub mode: u32,
    /// File bytes or symlink-target identity; absent for directories.
    pub digest: Option<ObjectDigest>,
}

/// Node kinds accepted in immutable trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeEntryKind {
    /// Directory.
    Directory,
    /// Regular file.
    File,
    /// Symbolic link stored as raw target bytes.
    Symlink,
}

pub(super) fn put(store: &CasStore, root: &Path) -> Result<ObjectDigest, CasError> {
    build(Some(store), root)
}

pub(super) fn digest(root: &Path) -> Result<ObjectDigest, CasError> {
    build(None, root)
}

fn build(store: Option<&CasStore>, root: &Path) -> Result<ObjectDigest, CasError> {
    let mut pending = vec![PathBuf::new()];
    let mut entries = Vec::new();
    while let Some(relative) = pending.pop() {
        let directory = root.join(&relative);
        let mut children = fs::read_dir(&directory)
            .map_err(|source| io_error(&directory, source))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| io_error(&directory, source))?;
        children.sort_by(|left, right| {
            left.file_name()
                .as_bytes()
                .cmp(right.file_name().as_bytes())
        });
        for child in children.into_iter().rev() {
            let child_relative = relative.join(child.file_name());
            let path = child.path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
            let (kind, mode, digest) = if metadata.file_type().is_dir() {
                pending.push(child_relative.clone());
                (TreeEntryKind::Directory, metadata.mode() & 0o777, None)
            } else if metadata.file_type().is_file() {
                let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
                let digest = ObjectDigest::of_bytes(&bytes);
                if let Some(store) = store {
                    store.put_verified(&digest, &bytes)?;
                }
                (TreeEntryKind::File, metadata.mode() & 0o777, Some(digest))
            } else if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path).map_err(|source| io_error(&path, source))?;
                let bytes = target.as_os_str().as_bytes();
                let digest = ObjectDigest::of_bytes(bytes);
                if let Some(store) = store {
                    store.put_verified(&digest, bytes)?;
                }
                (TreeEntryKind::Symlink, 0, Some(digest))
            } else {
                return Err(CasError::InvalidTree {
                    path: child_relative.display().to_string(),
                    reason: "only directories, regular files, and symbolic links are allowed"
                        .to_string(),
                });
            };
            entries.push(TreeEntry {
                path_hex: encode_path(&child_relative),
                kind,
                mode,
                digest,
            });
            if entries.len() > MAX_TREE_ENTRIES {
                return Err(CasError::InvalidTree {
                    path: root.display().to_string(),
                    reason: format!("tree exceeds the {MAX_TREE_ENTRIES}-entry limit"),
                });
            }
        }
    }
    entries.sort_by(|left, right| left.path_hex.cmp(&right.path_hex));
    let manifest = TreeManifest {
        schema_version: 1,
        entries,
    };
    let bytes = serde_json::to_vec(&manifest).map_err(CasError::TreeJson)?;
    let digest = ObjectDigest::of_bytes(&bytes);
    if let Some(store) = store {
        store.put_verified(&digest, &bytes)?;
        store.catalog.record_tree(&digest, &digest)?;
    }
    Ok(digest)
}

pub(super) fn materialize(
    store: &CasStore,
    digest: &ObjectDigest,
    destination: &Path,
) -> Result<(), CasError> {
    let bytes = store
        .read_verified(digest)?
        .ok_or_else(|| CasError::InvalidTree {
            path: digest.to_string(),
            reason: "manifest object is missing".to_string(),
        })?;
    let manifest: TreeManifest = serde_json::from_slice(&bytes).map_err(CasError::TreeJson)?;
    if manifest.schema_version != 1 {
        return Err(CasError::InvalidTree {
            path: digest.to_string(),
            reason: format!(
                "unsupported tree schema version {}",
                manifest.schema_version
            ),
        });
    }
    let mut directory_modes = Vec::new();
    for entry in manifest.entries {
        let relative = decode_path(&entry.path_hex)?;
        validate_relative(&relative)?;
        let target = destination.join(&relative);
        match entry.kind {
            TreeEntryKind::Directory => {
                fs::create_dir_all(&target).map_err(|source| io_error(&target, source))?;
                directory_modes.push((target, entry.mode));
            }
            TreeEntryKind::File => {
                let payload = payload(store, &entry, &relative)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
                }
                fs::write(&target, payload).map_err(|source| io_error(&target, source))?;
                fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode))
                    .map_err(|source| io_error(&target, source))?;
            }
            TreeEntryKind::Symlink => {
                let payload = payload(store, &entry, &relative)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
                }
                std::os::unix::fs::symlink(std::ffi::OsString::from_vec(payload), &target)
                    .map_err(|source| io_error(&target, source))?;
            }
        }
    }
    directory_modes.sort_by(|left, right| {
        right
            .0
            .components()
            .count()
            .cmp(&left.0.components().count())
    });
    for (directory, mode) in directory_modes {
        fs::set_permissions(&directory, fs::Permissions::from_mode(mode))
            .map_err(|source| io_error(&directory, source))?;
    }
    Ok(())
}

fn payload(store: &CasStore, entry: &TreeEntry, relative: &Path) -> Result<Vec<u8>, CasError> {
    let digest = entry.digest.as_ref().ok_or_else(|| CasError::InvalidTree {
        path: relative.display().to_string(),
        reason: "payload identity is missing".to_string(),
    })?;
    store
        .read_verified(digest)?
        .ok_or_else(|| CasError::InvalidTree {
            path: relative.display().to_string(),
            reason: format!("payload {digest} is missing"),
        })
}

fn validate_relative(path: &Path) -> Result<(), CasError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CasError::InvalidTree {
            path: path.display().to_string(),
            reason: "path must be a non-empty relative path".to_string(),
        });
    }
    Ok(())
}

fn encode_path(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

fn decode_path(encoded: &str) -> Result<PathBuf, CasError> {
    if !encoded.len().is_multiple_of(2) || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CasError::InvalidTree {
            path: encoded.to_string(),
            reason: "path encoding is malformed".to_string(),
        });
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let text = std::str::from_utf8(pair).map_err(|_| CasError::InvalidTree {
            path: encoded.to_string(),
            reason: "path encoding is malformed".to_string(),
        })?;
        let byte = u8::from_str_radix(text, 16).map_err(|_| CasError::InvalidTree {
            path: encoded.to_string(),
            reason: "path encoding is malformed".to_string(),
        })?;
        bytes.push(byte);
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

fn io_error(path: &Path, source: std::io::Error) -> CasError {
    CasError::Io {
        path: path.display().to_string(),
        source,
    }
}
