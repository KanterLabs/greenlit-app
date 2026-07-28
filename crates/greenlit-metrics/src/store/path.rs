//! Component-safe opening of the local metrics directory and NDJSON file.

use std::fs::File;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;

use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
use rustix::io::Errno;

use super::MetricsStore;
use crate::error::MetricsError;

impl MetricsStore {
    pub(super) fn open_for_append(&self) -> Result<File, MetricsError> {
        let flags = OFlags::CREATE
            | OFlags::RDWR
            | OFlags::APPEND
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK;
        let fd = if let Some(home) = &self.default_home {
            let directory = self.open_default_metrics_dir(home, true)?.ok_or_else(|| {
                MetricsError::CreateDir {
                    path: self
                        .file_path
                        .parent()
                        .unwrap_or(Path::new("."))
                        .to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "metrics directory was not created",
                    ),
                }
            })?;
            openat(&directory, "runs.ndjson", flags, Mode::RUSR | Mode::WUSR)
        } else {
            if let Some(parent) = self.file_path.parent() {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(true).mode(0o700);
                builder
                    .create(parent)
                    .map_err(|source| MetricsError::CreateDir {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                reject_broad_mode(
                    parent,
                    &std::fs::metadata(parent).map_err(|source| MetricsError::ReadFile {
                        path: parent.to_path_buf(),
                        source,
                    })?,
                )?;
            }
            open(&self.file_path, flags, Mode::RUSR | Mode::WUSR)
        }
        .map_err(|source| self.open_write_error(source))?;
        let file = self.regular_file(File::from(fd))?;
        reject_broad_mode(
            &self.file_path,
            &file.metadata().map_err(|source| MetricsError::ReadFile {
                path: self.file_path.clone(),
                source,
            })?,
        )?;
        Ok(file)
    }

    pub(super) fn open_for_read(&self) -> Result<Option<File>, MetricsError> {
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        let opened = if let Some(home) = &self.default_home {
            let Some(directory) = self.open_default_metrics_dir(home, false)? else {
                return Ok(None);
            };
            openat(&directory, "runs.ndjson", flags, Mode::empty())
        } else {
            open(&self.file_path, flags, Mode::empty())
        };
        match opened {
            Ok(fd) => self.regular_file(File::from(fd)).map(Some),
            Err(Errno::NOENT) => Ok(None),
            Err(Errno::LOOP | Errno::NOTDIR) => Err(MetricsError::InvalidPathComponent {
                path: self.file_path.clone(),
            }),
            Err(source) => Err(MetricsError::ReadFile {
                path: self.file_path.clone(),
                source: source.into(),
            }),
        }
    }

    fn open_default_metrics_dir(
        &self,
        home: &File,
        create: bool,
    ) -> Result<Option<File>, MetricsError> {
        let home_path = self
            .file_path
            .ancestors()
            .nth(3)
            .unwrap_or_else(|| Path::new("."));
        let Some(litci) = open_directory_component(home, home_path, ".litci", create)? else {
            return Ok(None);
        };
        reject_broad_mode(
            &home_path.join(".litci"),
            &litci.metadata().map_err(|source| MetricsError::ReadFile {
                path: home_path.join(".litci"),
                source,
            })?,
        )?;
        open_directory_component(&litci, &home_path.join(".litci"), "metrics", create)
    }

    fn regular_file(&self, file: File) -> Result<File, MetricsError> {
        let metadata = file.metadata().map_err(|source| MetricsError::ReadFile {
            path: self.file_path.clone(),
            source,
        })?;
        if metadata.is_file() {
            Ok(file)
        } else {
            Err(MetricsError::InvalidPathComponent {
                path: self.file_path.clone(),
            })
        }
    }

    fn open_write_error(&self, source: Errno) -> MetricsError {
        if matches!(source, Errno::LOOP | Errno::NOTDIR) {
            MetricsError::InvalidPathComponent {
                path: self.file_path.clone(),
            }
        } else {
            MetricsError::OpenForWrite {
                path: self.file_path.clone(),
                source: source.into(),
            }
        }
    }
}

fn open_directory_component(
    parent: &File,
    parent_path: &Path,
    name: &str,
    create: bool,
) -> Result<Option<File>, MetricsError> {
    let flags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    loop {
        match openat(parent, name, flags, Mode::empty()) {
            Ok(fd) => {
                let file = File::from(fd);
                reject_broad_mode(
                    &parent_path.join(name),
                    &file.metadata().map_err(|source| MetricsError::ReadFile {
                        path: parent_path.join(name),
                        source,
                    })?,
                )?;
                return Ok(Some(file));
            }
            Err(Errno::NOENT) if !create => return Ok(None),
            Err(Errno::NOENT) => {
                let path = parent_path.join(name);
                match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                    Ok(()) | Err(Errno::EXIST) => {}
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

fn reject_broad_mode(path: &Path, metadata: &std::fs::Metadata) -> Result<(), MetricsError> {
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 == 0 {
        Ok(())
    } else {
        Err(MetricsError::UnsafePermissions {
            path: path.to_path_buf(),
            mode,
        })
    }
}
