//! The copy-in isolation fallback.
//!
//! Where unprivileged overlayfs is unavailable, the read-only checkout is
//! copied into a writable container-local workspace instead, so the job command
//! still sees a writable tree it can freely modify or delete without touching
//! the host (whose bind stays read-only at the Docker level regardless). Only
//! regular files, directories, and symlinks are reproduced; special filesystem
//! nodes (devices, FIFOs, sockets) are skipped rather than recreated.

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use crate::cli::FileCopyPolicy;
use crate::error::InitError;

/// Stable marker carrying the measured result of one complete copy-in.
pub const COPY_REPORT_MARKER: &str = "GREENLIT_INIT_COPY";

/// How many copied entries between progress callbacks. Fine enough that a
/// multi-gigabyte tree reports every few hundred milliseconds, coarse enough
/// that the callback (a status-file write) is noise against the copy itself.
const PROGRESS_EVERY: u64 = 512;

/// Resource ceilings for the bounded copy fallback.
const MAX_COPY_FILES: u64 = 2_000_000;
const MAX_COPY_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

/// Cumulative copy-in measurements, including the actual strategy used for
/// every regular file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CopyStats {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    pub(crate) reflink_files: u64,
    pub(crate) bounded_stream_files: u64,
}

impl CopyStats {
    pub(crate) fn report(self) -> String {
        format!(
            "{COPY_REPORT_MARKER} v=1 files={} bytes={} reflink_files={} bounded_stream_files={}",
            self.files, self.bytes, self.reflink_files, self.bounded_stream_files
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileCopyStrategy {
    Reflink,
    BoundedStream,
}

struct FileCopyResult {
    bytes: u64,
    strategy: FileCopyStrategy,
}

/// Copy the entire tree rooted at `lower` into `workspace`, creating
/// `workspace` if needed. `on_progress` receives cumulative [`CopyStats`]
/// every [`PROGRESS_EVERY`] entries and once at the end — monotone running
/// totals only; a grand total would cost a second full walk.
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
    policy: FileCopyPolicy,
    on_progress: &mut dyn FnMut(CopyStats),
) -> Result<CopyStats, InitError> {
    populate_with_limits(
        lower,
        workspace,
        policy,
        on_progress,
        MAX_COPY_FILES,
        MAX_COPY_BYTES,
    )
}

fn populate_with_limits(
    lower: &Path,
    workspace: &Path,
    policy: FileCopyPolicy,
    on_progress: &mut dyn FnMut(CopyStats),
    max_files: u64,
    max_bytes: u64,
) -> Result<CopyStats, InitError> {
    fs::create_dir_all(workspace).map_err(|source| InitError::PrepareDir {
        path: workspace.to_path_buf(),
        source,
    })?;

    let mut stats = CopyStats::default();
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
                stack.push((src.clone(), dst));
            } else if file_type.is_symlink() {
                let target = fs::read_link(&src).map_err(|source| InitError::CopyIn {
                    path: src.clone(),
                    source,
                })?;
                std::os::unix::fs::symlink(&target, &dst).map_err(|source| InitError::CopyIn {
                    path: dst.clone(),
                    source,
                })?;
                stats.files += 1;
            } else if file_type.is_file() {
                let copied =
                    clone_or_copy(&src, &dst, policy).map_err(|source| InitError::CopyIn {
                        path: src.clone(),
                        source,
                    })?;
                stats.bytes =
                    stats
                        .bytes
                        .checked_add(copied.bytes)
                        .ok_or_else(|| InitError::CopyIn {
                            path: src.clone(),
                            source: limit_error("byte count overflowed"),
                        })?;
                stats.files += 1;
                match copied.strategy {
                    FileCopyStrategy::Reflink => stats.reflink_files += 1,
                    FileCopyStrategy::BoundedStream => stats.bounded_stream_files += 1,
                }
            } else if is_special(&file_type) {
                // Devices, FIFOs, and sockets are runtime artifacts, not source
                // content — skip them rather than attempt to recreate them.
                continue;
            }
            if stats.files > max_files {
                return Err(InitError::CopyIn {
                    path: src.clone(),
                    source: limit_error("file count exceeded the workspace copy limit"),
                });
            }
            if stats.bytes > max_bytes {
                return Err(InitError::CopyIn {
                    path: src,
                    source: limit_error("byte count exceeded the workspace copy limit"),
                });
            }
            entries_seen += 1;
            if entries_seen.is_multiple_of(PROGRESS_EVERY) {
                on_progress(stats);
            }
        }
    }
    on_progress(stats);
    Ok(stats)
}

fn clone_or_copy(
    source_path: &Path,
    destination_path: &Path,
    policy: FileCopyPolicy,
) -> std::io::Result<FileCopyResult> {
    let source = File::open(source_path)?;
    let destination = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(destination_path)?;
    match policy {
        FileCopyPolicy::Auto => match reflink(&source, &destination) {
            Ok(result) => Ok(result),
            Err(_) => {
                drop(destination);
                drop(source);
                bounded_stream_copy(source_path, destination_path)
            }
        },
        FileCopyPolicy::RequireReflink => reflink(&source, &destination),
        FileCopyPolicy::BoundedStream => {
            drop(destination);
            drop(source);
            bounded_stream_copy(source_path, destination_path)
        }
    }
}

fn reflink(source: &File, destination: &File) -> std::io::Result<FileCopyResult> {
    rustix::fs::ioctl_ficlone(destination, source)?;
    let metadata = source.metadata()?;
    destination.set_permissions(metadata.permissions())?;
    Ok(FileCopyResult {
        bytes: metadata.len(),
        strategy: FileCopyStrategy::Reflink,
    })
}

fn bounded_stream_copy(
    source_path: &Path,
    destination_path: &Path,
) -> std::io::Result<FileCopyResult> {
    let mut source = File::open(source_path)?;
    let metadata = source.metadata()?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(destination_path)?;
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; STREAM_BUFFER_BYTES];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        destination.write_all(&buffer[..read])?;
        bytes = bytes.checked_add(read as u64).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "streamed file byte count overflowed",
            )
        })?;
    }
    destination.set_permissions(metadata.permissions())?;
    Ok(FileCopyResult {
        bytes,
        strategy: FileCopyStrategy::BoundedStream,
    })
}

fn limit_error(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::FileTooLarge, message)
}

/// Whether a file type is a special node (device, FIFO, or socket) that the
/// copy deliberately skips.
fn is_special(file_type: &fs::FileType) -> bool {
    file_type.is_block_device()
        || file_type.is_char_device()
        || file_type.is_fifo()
        || file_type.is_socket()
}
