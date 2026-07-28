//! The exported overlay diff: reading its change list and applying it to the
//! host working tree.
//!
//! `--write-back` exports the overlay **upper** layer (see
//! [`crate::isolation::OVERLAY_UPPER_EXPORT_PATH`]) as a tar. The upper contains
//! exactly what the workflow changed: created/modified files and directories as
//! themselves, and deletions as overlayfs *whiteouts* — character devices with
//! device number 0 (see the Linux overlayfs docs, "whiteouts and opaque
//! directories"). Docker's archive endpoint roots every entry at the exported
//! directory's basename, which is stripped here to yield workspace-relative
//! paths.

use std::path::{Component, Path, PathBuf};

use tar::{Archive, EntryType};

use super::error::WriteBackError;
use super::host_fs::HostRoot;

/// One change the workflow made to its workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Workspace-relative path of the changed entry.
    pub path: PathBuf,
    /// What kind of change it is.
    pub kind: ChangeKind,
}

/// The nature of a change, for the confirmation listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A file, directory, or symlink created or modified by the workflow.
    AddedOrModified,
    /// A path the workflow deleted (an overlayfs whiteout).
    Deleted,
}

impl ChangeKind {
    /// A single-character sigil for the confirmation listing (`+`/`-`).
    #[must_use]
    pub fn sigil(self) -> char {
        match self {
            ChangeKind::AddedOrModified => '+',
            ChangeKind::Deleted => '-',
        }
    }
}

/// The exported overlay upper layer, as an uncompressed tar.
#[derive(Debug, Clone)]
pub struct OverlayDiff {
    tar: Vec<u8>,
}

impl OverlayDiff {
    /// Wrap the raw tar bytes exported from the container.
    #[must_use]
    pub fn new(tar: Vec<u8>) -> Self {
        OverlayDiff { tar }
    }

    /// The list of workspace-relative changes, for review before applying.
    ///
    /// # Errors
    ///
    /// Returns [`WriteBackError::Read`] if the archive cannot be parsed, or
    /// [`WriteBackError::UnsafePath`] if an entry escapes the workspace root.
    pub fn changes(&self) -> Result<Vec<Change>, WriteBackError> {
        let mut changes = Vec::new();
        let mut archive = Archive::new(self.tar.as_slice());
        let entries = archive.entries().map_err(WriteBackError::Read)?;
        for entry in entries {
            let entry = entry.map_err(WriteBackError::Read)?;
            let raw = entry.path().map_err(WriteBackError::Read)?.into_owned();
            let Some(rel) = workspace_relative(&raw)? else {
                continue;
            };
            let kind = if entry.header().entry_type() == EntryType::Char {
                ChangeKind::Deleted
            } else {
                ChangeKind::AddedOrModified
            };
            changes.push(Change { path: rel, kind });
        }
        Ok(changes)
    }

    /// Apply the diff to `host_root`, writing created/modified entries and
    /// removing whiteouts.
    ///
    /// Only regular files, directories, and symlinks are reproduced; other node
    /// types are skipped. Every destination is re-validated to stay within
    /// `host_root` lexically (`workspace_relative`) *and* every write goes
    /// through `HostRoot`, which resolves each path component by
    /// descriptor rather than through the kernel's ordinary (symlink-
    /// following) path lookup — so a malformed archive, or a host-side
    /// symlink the repository itself checked in, cannot make a write land
    /// outside the working tree (finding: write-back symlink traversal; see
    /// `host_fs`'s module doc for the full rationale).
    ///
    /// # Errors
    ///
    /// Returns [`WriteBackError::Read`] on archive-parse failure,
    /// [`WriteBackError::UnsafePath`] on an escaping entry, or
    /// [`WriteBackError::Apply`] on an I/O failure writing the host tree.
    pub fn apply(&self, host_root: &Path) -> Result<(), WriteBackError> {
        let root = HostRoot::open(host_root).map_err(|source| WriteBackError::Apply {
            path: host_root.to_path_buf(),
            source,
        })?;
        let mut archive = Archive::new(self.tar.as_slice());
        let entries = archive.entries().map_err(WriteBackError::Read)?;
        for entry in entries {
            let mut entry = entry.map_err(WriteBackError::Read)?;
            let raw = entry.path().map_err(WriteBackError::Read)?.into_owned();
            let Some(rel) = workspace_relative(&raw)? else {
                continue;
            };
            let result = match entry.header().entry_type() {
                EntryType::Char => root.remove_path(&rel),
                EntryType::Directory => root.ensure_dir(&rel),
                EntryType::Symlink => {
                    let target = entry
                        .link_name()
                        .map_err(WriteBackError::Read)?
                        .map(|t| t.into_owned())
                        .unwrap_or_default();
                    root.write_symlink(&rel, &target)
                }
                EntryType::Regular | EntryType::GNUSparse => root.write_file(&rel, &mut entry),
                // Block devices, FIFOs, sockets, and metadata-only entries are
                // not workspace content — skip them.
                _ => Ok(()),
            };
            result.map_err(|source| apply_err(&host_root.join(&rel), source))?;
        }
        Ok(())
    }
}

/// Strip the archive-root component and validate the remainder stays within the
/// workspace. Returns `None` for the bare root entry (nothing to apply).
fn workspace_relative(raw: &Path) -> Result<Option<PathBuf>, WriteBackError> {
    let mut components = raw.components();
    // Drop the archive root (the exported directory's basename).
    components.next();
    let mut rel = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => rel.push(part),
            // Any non-normal component (`..`, absolute, prefix) is an escape.
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(WriteBackError::UnsafePath {
                    path: raw.to_path_buf(),
                });
            }
            Component::CurDir => {}
        }
    }
    if rel.as_os_str().is_empty() {
        Ok(None)
    } else {
        Ok(Some(rel))
    }
}

/// Build an [`WriteBackError::Apply`] for a host path.
fn apply_err(dest: &Path, source: std::io::Error) -> WriteBackError {
    WriteBackError::Apply {
        path: dest.to_path_buf(),
        source,
    }
}
