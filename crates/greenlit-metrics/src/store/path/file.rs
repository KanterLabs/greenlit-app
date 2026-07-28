//! Exact private file creation for metrics stores.

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{Mode, OFlags, fchmod, openat};
use rustix::io::Errno;

use crate::error::MetricsError;

pub(super) fn open_private_for_append(
    parent: &File,
    path: &Path,
    name: &OsStr,
) -> Result<File, MetricsError> {
    let existing =
        OFlags::RDWR | OFlags::APPEND | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    loop {
        match openat(parent, name, existing, Mode::empty()) {
            Ok(fd) => {
                let file = File::from(fd);
                validate_private_file(path, &file)?;
                return Ok(file);
            }
            Err(Errno::NOENT) => match openat(
                parent,
                name,
                existing | OFlags::CREATE | OFlags::EXCL,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(fd) => return normalize_new_file(path, File::from(fd)),
                Err(Errno::EXIST) => {}
                Err(Errno::LOOP | Errno::NOTDIR) => {
                    return Err(MetricsError::InvalidPathComponent {
                        path: path.to_path_buf(),
                    });
                }
                Err(source) => {
                    return Err(MetricsError::OpenForWrite {
                        path: path.to_path_buf(),
                        source: source.into(),
                    });
                }
            },
            Err(Errno::LOOP | Errno::NOTDIR) => {
                return Err(MetricsError::InvalidPathComponent {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(MetricsError::OpenForWrite {
                    path: path.to_path_buf(),
                    source: source.into(),
                });
            }
        }
    }
}

pub(super) fn open_private_for_read(
    parent: &File,
    path: &Path,
    name: &OsStr,
) -> Result<Option<File>, MetricsError> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    match openat(parent, name, flags, Mode::empty()) {
        Ok(fd) => {
            let file = File::from(fd);
            validate_private_file(path, &file)?;
            Ok(Some(file))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => Err(MetricsError::InvalidPathComponent {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(MetricsError::ReadFile {
            path: path.to_path_buf(),
            source: source.into(),
        }),
    }
}

fn normalize_new_file(path: &Path, file: File) -> Result<File, MetricsError> {
    let metadata = file.metadata().map_err(|source| MetricsError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(MetricsError::InvalidPathComponent {
            path: path.to_path_buf(),
        });
    }
    if mode & !0o600 != 0 {
        return Err(MetricsError::UnsafePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(|source| MetricsError::OpenForWrite {
        path: path.to_path_buf(),
        source: source.into(),
    })?;
    validate_private_file(path, &file)?;
    Ok(file)
}

fn validate_private_file(path: &Path, file: &File) -> Result<(), MetricsError> {
    let metadata = file.metadata().map_err(|source| MetricsError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(MetricsError::InvalidPathComponent {
            path: path.to_path_buf(),
        });
    }
    if mode != 0o600 {
        return Err(MetricsError::UnsafePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

fn validate_owner(path: &Path, metadata: &std::fs::Metadata) -> Result<(), MetricsError> {
    let expected = rustix::process::getuid().as_raw();
    if metadata.uid() == expected {
        Ok(())
    } else {
        Err(MetricsError::UnsafeOwner {
            path: path.to_path_buf(),
            owner: metadata.uid(),
            expected,
        })
    }
}
