//! Exact private directory creation for metrics stores.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, chmod, mkdirat, openat, openat2};
use rustix::io::Errno;

use crate::error::MetricsError;

pub(super) fn open_component(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
    create: bool,
) -> Result<Option<File>, MetricsError> {
    let flags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    loop {
        match openat(parent, name, flags, Mode::empty()) {
            Ok(fd) => {
                let file = File::from(fd);
                validate_private_directory(&parent_path.join(name), &file)?;
                return Ok(Some(file));
            }
            Err(Errno::NOENT) if !create => return Ok(None),
            Err(Errno::NOENT) => {
                let path = parent_path.join(name);
                match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                    Ok(()) => return open_new_directory(parent, parent_path, name).map(Some),
                    Err(Errno::EXIST) => {}
                    Err(source) => {
                        return Err(MetricsError::CreateDir {
                            path,
                            source: source.into(),
                        });
                    }
                }
            }
            Err(Errno::LOOP | Errno::NOTDIR) => {
                return Err(MetricsError::InvalidPathComponent {
                    path: parent_path.join(name),
                });
            }
            Err(source) => {
                return Err(MetricsError::ReadFile {
                    path: parent_path.join(name),
                    source: source.into(),
                });
            }
        }
    }
}

pub(super) fn open_explicit_parent(
    parent: &Path,
    create: bool,
) -> Result<Option<File>, MetricsError> {
    if !create {
        return match open_directory_path(parent) {
            Ok(directory) => {
                validate_private_directory(parent, &directory)?;
                Ok(Some(directory))
            }
            Err(MetricsError::ReadFile { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        };
    }

    let mut cursor = parent.to_path_buf();
    let mut missing = Vec::<OsString>::new();
    loop {
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(MetricsError::InvalidPathComponent { path: cursor });
                }
                let mut directory = open_directory_path(&cursor)?;
                if missing.is_empty() {
                    validate_private_directory(parent, &directory)?;
                    return Ok(Some(directory));
                }
                let mut directory_path = cursor;
                for name in missing.iter().rev() {
                    directory = open_component(&directory, &directory_path, name, true)?
                        .ok_or_else(|| MetricsError::CreateDir {
                            path: directory_path.join(name),
                            source: std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "metrics directory component was not created",
                            ),
                        })?;
                    directory_path.push(name);
                }
                return Ok(Some(directory));
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor.file_name().ok_or_else(|| MetricsError::CreateDir {
                    path: parent.to_path_buf(),
                    source,
                })?;
                missing.push(name.to_os_string());
                cursor = cursor
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
            }
            Err(source) => {
                return Err(MetricsError::ReadFile {
                    path: cursor,
                    source,
                });
            }
        }
    }
}

fn open_new_directory(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
) -> Result<File, MetricsError> {
    let path = parent_path.join(name);
    let inspected = openat(
        parent,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|source| MetricsError::ReadFile {
        path: path.clone(),
        source: source.into(),
    })?;
    let before = inspected
        .metadata()
        .map_err(|source| MetricsError::ReadFile {
            path: path.clone(),
            source,
        })?;
    validate_owner(&path, &before)?;
    let mode = before.mode() & 0o7777;
    if !before.is_dir() || mode & !0o2700 != 0 {
        return Err(MetricsError::UnsafePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    normalize_new_directory(&path, &inspected)?;
    let directory = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|source| MetricsError::ReadFile {
        path: path.clone(),
        source: source.into(),
    })?;
    let after = directory
        .metadata()
        .map_err(|source| MetricsError::ReadFile {
            path: path.clone(),
            source,
        })?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(MetricsError::InvalidPathComponent { path });
    }
    validate_private_directory(&path, &directory)?;
    Ok(directory)
}

fn open_directory_path(path: &Path) -> Result<File, MetricsError> {
    openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
    )
    .map(File::from)
    .map_err(|source| match source {
        Errno::LOOP | Errno::NOTDIR => MetricsError::InvalidPathComponent {
            path: path.to_path_buf(),
        },
        source => MetricsError::ReadFile {
            path: path.to_path_buf(),
            source: source.into(),
        },
    })
}

fn normalize_new_directory(path: &Path, directory: &File) -> Result<(), MetricsError> {
    let descriptor = format!("/proc/self/fd/{}", directory.as_raw_fd());
    chmod(descriptor, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(|source| {
        MetricsError::CreateDir {
            path: path.to_path_buf(),
            source: source.into(),
        }
    })
}

fn validate_private_directory(path: &Path, directory: &File) -> Result<(), MetricsError> {
    let metadata = directory
        .metadata()
        .map_err(|source| MetricsError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir() || mode != 0o700 {
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
