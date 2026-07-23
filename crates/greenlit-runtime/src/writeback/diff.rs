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
    /// `host_root` before any write, so a malformed archive cannot touch the
    /// host outside the working tree.
    ///
    /// # Errors
    ///
    /// Returns [`WriteBackError::Read`] on archive-parse failure,
    /// [`WriteBackError::UnsafePath`] on an escaping entry, or
    /// [`WriteBackError::Apply`] on an I/O failure writing the host tree.
    pub fn apply(&self, host_root: &Path) -> Result<(), WriteBackError> {
        let mut archive = Archive::new(self.tar.as_slice());
        let entries = archive.entries().map_err(WriteBackError::Read)?;
        for entry in entries {
            let mut entry = entry.map_err(WriteBackError::Read)?;
            let raw = entry.path().map_err(WriteBackError::Read)?.into_owned();
            let Some(rel) = workspace_relative(&raw)? else {
                continue;
            };
            let dest = host_root.join(&rel);
            match entry.header().entry_type() {
                EntryType::Char => remove_path(&dest)?,
                EntryType::Directory => ensure_dir(&dest)?,
                EntryType::Symlink => {
                    let target = entry
                        .link_name()
                        .map_err(WriteBackError::Read)?
                        .map(|t| t.into_owned())
                        .unwrap_or_default();
                    write_symlink(&dest, &target)?;
                }
                EntryType::Regular | EntryType::GNUSparse => write_file(&dest, &mut entry)?,
                // Block devices, FIFOs, sockets, and metadata-only entries are
                // not workspace content — skip them.
                _ => {}
            }
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

/// Remove a whiteout target from the host (file or directory tree). A missing
/// target is not an error — the deletion is already reflected.
fn remove_path(dest: &Path) -> Result<(), WriteBackError> {
    let meta = match std::fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(apply_err(dest, e)),
    };
    let result = if meta.is_dir() {
        std::fs::remove_dir_all(dest)
    } else {
        std::fs::remove_file(dest)
    };
    result.map_err(|e| apply_err(dest, e))
}

/// Create a directory (and parents) on the host.
fn ensure_dir(dest: &Path) -> Result<(), WriteBackError> {
    std::fs::create_dir_all(dest).map_err(|e| apply_err(dest, e))
}

/// Write a regular-file entry to the host, creating parent directories.
fn write_file(dest: &Path, entry: &mut dyn std::io::Read) -> Result<(), WriteBackError> {
    if let Some(parent) = dest.parent() {
        ensure_dir(parent)?;
    }
    let mut file = std::fs::File::create(dest).map_err(|e| apply_err(dest, e))?;
    std::io::copy(entry, &mut file).map_err(|e| apply_err(dest, e))?;
    Ok(())
}

/// Recreate a symlink on the host, replacing any existing entry at the path.
fn write_symlink(dest: &Path, target: &Path) -> Result<(), WriteBackError> {
    if let Some(parent) = dest.parent() {
        ensure_dir(parent)?;
    }
    // Replace an existing node so a re-run is idempotent.
    if std::fs::symlink_metadata(dest).is_ok() {
        remove_path(dest)?;
    }
    std::os::unix::fs::symlink(target, dest).map_err(|e| apply_err(dest, e))
}

/// Build an [`WriteBackError::Apply`] for a host path.
fn apply_err(dest: &Path, source: std::io::Error) -> WriteBackError {
    WriteBackError::Apply {
        path: dest.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tar::{Builder, Header};

    /// Build a tar rooted at `upper/` (mimicking Docker's archive of the upper
    /// dir) from `(name, kind, payload)` tuples.
    fn upper_tar(entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        for (name, kind, payload) in entries {
            let path = format!("upper/{name}");
            let mut header = Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            match kind {
                EntryType::Symlink => {
                    header.set_size(0);
                    header.set_cksum();
                    let target = std::str::from_utf8(payload).expect("utf8 link target");
                    builder
                        .append_link(&mut header, &path, target)
                        .expect("append symlink");
                }
                EntryType::Char => {
                    header.set_size(0);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, &path, std::io::empty())
                        .expect("append whiteout");
                }
                _ => {
                    header.set_size(payload.len() as u64);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, &path, *payload)
                        .expect("append data");
                }
            }
        }
        builder.into_inner().expect("finish tar")
    }

    #[test]
    fn lists_additions_and_deletions_stripped_to_workspace_paths() {
        let tar = upper_tar(&[
            ("new.txt", EntryType::Regular, b"hi"),
            ("gone.txt", EntryType::Char, b""),
        ]);
        let changes = OverlayDiff::new(tar).changes().expect("changes");
        assert!(changes.contains(&Change {
            path: PathBuf::from("new.txt"),
            kind: ChangeKind::AddedOrModified,
        }));
        assert!(changes.contains(&Change {
            path: PathBuf::from("gone.txt"),
            kind: ChangeKind::Deleted,
        }));
    }

    #[test]
    fn apply_writes_files_and_removes_whiteouts() {
        let dir = tempdir();
        // A file that already exists and will be deleted by a whiteout.
        std::fs::write(dir.join("gone.txt"), b"old").expect("seed");
        let tar = upper_tar(&[
            ("dir/added.txt", EntryType::Regular, b"content"),
            ("gone.txt", EntryType::Char, b""),
        ]);
        OverlayDiff::new(tar).apply(&dir).expect("apply");
        assert_eq!(
            std::fs::read(dir.join("dir/added.txt")).expect("read added"),
            b"content"
        );
        assert!(!dir.join("gone.txt").exists(), "whiteout removed the file");
    }

    #[test]
    fn refuses_paths_that_escape_the_workspace() {
        // A hostile archive whose entry escapes the workspace after the root is
        // stripped. `tar::Builder` refuses to *write* a `..` path, so the header
        // is hand-crafted to reproduce what a malicious daemon response could
        // contain — the guard, not the writer, is what must reject it.
        let tar = raw_tar_entry("upper/../escape");
        let err = OverlayDiff::new(tar).changes().unwrap_err();
        assert!(matches!(err, WriteBackError::UnsafePath { .. }));
    }

    /// Assemble a single-entry USTAR archive with an arbitrary (possibly
    /// unsafe) name, bypassing `tar::Builder`'s path validation.
    fn raw_tar_entry(name: &str) -> Vec<u8> {
        let mut block = [0u8; 512];
        let name_bytes = name.as_bytes();
        block[..name_bytes.len()].copy_from_slice(name_bytes);
        write_octal(&mut block[100..108], 0o644); // mode
        write_octal(&mut block[124..136], 0); // size (0 bytes of data)
        block[156] = b'0'; // typeflag: regular file
        block[257..263].copy_from_slice(b"ustar\0");
        block[263..265].copy_from_slice(b"00");
        // Checksum: sum of all bytes with the checksum field taken as spaces.
        block[148..156].fill(b' ');
        let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
        write_octal(&mut block[148..154], sum as u64);
        block[154] = 0;
        block[155] = b' ';
        // Header block followed by the two zero blocks that end an archive.
        let mut archive = block.to_vec();
        archive.extend_from_slice(&[0u8; 1024]);
        archive
    }

    /// Write `value` as a NUL-terminated octal field filling `field`.
    fn write_octal(field: &mut [u8], value: u64) {
        let digits = field.len() - 1;
        let text = format!("{value:0width$o}", width = digits);
        field[..digits].copy_from_slice(&text.as_bytes()[..digits]);
        field[digits] = 0;
    }

    /// A throwaway directory under the system temp dir, unique per test.
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "greenlit-writeback-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&base).expect("create tempdir");
        base
    }
}
