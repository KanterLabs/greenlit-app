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

/// How many copied entries between progress callbacks. Fine enough that a
/// multi-gigabyte tree reports every few hundred milliseconds, coarse enough
/// that the callback (a status-file write) is noise against the copy itself.
const PROGRESS_EVERY: u64 = 512;

/// Copy the entire tree rooted at `lower` into `workspace`, creating
/// `workspace` if needed. `on_progress` receives cumulative (files, bytes)
/// counts every [`PROGRESS_EVERY`] entries and once at the end — monotone
/// running totals only; a grand total would cost a second full walk.
///
/// Uses an explicit work stack rather than recursion so a deep repository
/// cannot overflow the stack.
///
/// # Errors
///
/// Returns [`InitError::CopyIn`] (with the path being processed) or
/// [`InitError::PrepareDir`] (for the workspace root) on any I/O failure.
pub(crate) fn populate(
    lower: &Path,
    workspace: &Path,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<(), InitError> {
    fs::create_dir_all(workspace).map_err(|source| InitError::PrepareDir {
        path: workspace.to_path_buf(),
        source,
    })?;

    let mut files: u64 = 0;
    let mut bytes: u64 = 0;
    let mut entries_seen: u64 = 0;

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
                files += 1;
            } else if file_type.is_file() {
                bytes += fs::copy(&src, &dst).map_err(|source| InitError::CopyIn {
                    path: src.clone(),
                    source,
                })?;
                files += 1;
            } else if is_special(&file_type) {
                // Devices, FIFOs, and sockets are runtime artifacts, not source
                // content — skip them rather than attempt to recreate them.
                continue;
            }
            entries_seen += 1;
            if entries_seen.is_multiple_of(PROGRESS_EVERY) {
                on_progress(files, bytes);
            }
        }
    }
    on_progress(files, bytes);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_reports_every_batch_and_a_final_total() {
        let lower = tempfile::tempdir().expect("lower");
        let workspace = tempfile::tempdir().expect("workspace");
        const FILES: u64 = 600; // one full 512-entry batch plus a tail
        for index in 0..FILES {
            fs::write(lower.path().join(format!("f{index}")), b"12345").expect("seed");
        }

        let mut reports: Vec<(u64, u64)> = Vec::new();
        populate(
            lower.path(),
            &workspace.path().join("ws"),
            &mut |files, bytes| {
                reports.push((files, bytes));
            },
        )
        .expect("copy");

        assert!(
            reports.len() >= 2,
            "a batch report plus the final report: {reports:?}"
        );
        assert_eq!(
            reports.last(),
            Some(&(FILES, FILES * 5)),
            "the final report carries the full tree's totals"
        );
        let monotone = reports.windows(2).all(|pair| pair[0] <= pair[1]);
        assert!(monotone, "counts only grow: {reports:?}");
    }

    #[test]
    fn symlinks_count_as_files_without_bytes() {
        let lower = tempfile::tempdir().expect("lower");
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(lower.path().join("real"), b"abc").expect("seed");
        std::os::unix::fs::symlink("real", lower.path().join("link")).expect("symlink");

        let mut last = (0, 0);
        populate(
            lower.path(),
            &workspace.path().join("ws"),
            &mut |files, bytes| {
                last = (files, bytes);
            },
        )
        .expect("copy");

        assert_eq!(last, (2, 3), "two entries, three bytes of file content");
    }
}
