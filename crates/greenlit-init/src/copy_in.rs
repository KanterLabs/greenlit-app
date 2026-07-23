//! The copy-in isolation fallback.
//!
//! Where unprivileged overlayfs is unavailable, the read-only checkout is
//! copied into a writable container-local workspace instead, so the job command
//! still sees a writable tree it can freely modify or delete without touching
//! the host (whose bind stays read-only at the Docker level regardless). Only
//! regular files, directories, and symlinks are reproduced; special filesystem
//! nodes (devices, FIFOs, sockets) are skipped rather than recreated.

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use crate::error::InitError;

/// Copy the entire tree rooted at `lower` into `workspace`, creating
/// `workspace` if needed.
///
/// Uses an explicit work stack rather than recursion so a deep repository
/// cannot overflow the stack.
///
/// # Errors
///
/// Returns [`InitError::CopyIn`] (with the path being processed) or
/// [`InitError::PrepareDir`] (for the workspace root) on any I/O failure.
pub(crate) fn populate(lower: &Path, workspace: &Path) -> Result<(), InitError> {
    fs::create_dir_all(workspace).map_err(|source| InitError::PrepareDir {
        path: workspace.to_path_buf(),
        source,
    })?;

    // Each item is (source dir, destination dir); process children iteratively.
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(lower.to_path_buf(), workspace.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        let entries = fs::read_dir(&src_dir).map_err(|source| InitError::CopyIn {
            path: src_dir.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| InitError::CopyIn {
                path: src_dir.clone(),
                source,
            })?;
            let src = entry.path();
            let dst = dst_dir.join(entry.file_name());
            let file_type = entry.file_type().map_err(|source| InitError::CopyIn {
                path: src.clone(),
                source,
            })?;

            if file_type.is_dir() {
                fs::create_dir_all(&dst).map_err(|source| InitError::CopyIn {
                    path: dst.clone(),
                    source,
                })?;
                stack.push((src, dst));
            } else if file_type.is_symlink() {
                let target = fs::read_link(&src).map_err(|source| InitError::CopyIn {
                    path: src.clone(),
                    source,
                })?;
                std::os::unix::fs::symlink(&target, &dst).map_err(|source| InitError::CopyIn {
                    path: dst.clone(),
                    source,
                })?;
            } else if file_type.is_file() {
                fs::copy(&src, &dst).map_err(|source| InitError::CopyIn {
                    path: src.clone(),
                    source,
                })?;
            } else if is_special(&file_type) {
                // Devices, FIFOs, and sockets are runtime artifacts, not source
                // content — skip them rather than attempt to recreate them.
                continue;
            }
        }
    }
    Ok(())
}

/// Whether a file type is a special node (device, FIFO, or socket) that the
/// copy deliberately skips.
fn is_special(file_type: &fs::FileType) -> bool {
    file_type.is_block_device()
        || file_type.is_char_device()
        || file_type.is_fifo()
        || file_type.is_socket()
}
