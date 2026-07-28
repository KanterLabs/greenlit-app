//! Component-safe opening of the local metrics directory and NDJSON file.

use std::fs::File;
use std::path::Path;

use super::MetricsStore;
use crate::error::MetricsError;

mod directory;
mod file;

impl MetricsStore {
    pub(super) fn open_for_append(&self) -> Result<File, MetricsError> {
        let (directory, name) = if let Some(home) = &self.default_home {
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
            (directory, std::ffi::OsStr::new("runs.ndjson"))
        } else {
            let (parent, name) = explicit_parent_and_name(&self.file_path)?;
            let directory = directory::open_explicit_parent(parent, true)?.ok_or_else(|| {
                MetricsError::CreateDir {
                    path: parent.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "metrics directory was not created",
                    ),
                }
            })?;
            (directory, name)
        };
        file::open_private_for_append(&directory, &self.file_path, name)
    }

    pub(super) fn open_for_read(&self) -> Result<Option<File>, MetricsError> {
        let (directory, name) = if let Some(home) = &self.default_home {
            let Some(directory) = self.open_default_metrics_dir(home, false)? else {
                return Ok(None);
            };
            (directory, std::ffi::OsStr::new("runs.ndjson"))
        } else {
            let (parent, name) = explicit_parent_and_name(&self.file_path)?;
            let Some(directory) = directory::open_explicit_parent(parent, false)? else {
                return Ok(None);
            };
            (directory, name)
        };
        file::open_private_for_read(&directory, &self.file_path, name)
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
        let Some(litci) = directory::open_component(home, home_path, ".litci".as_ref(), create)?
        else {
            return Ok(None);
        };
        directory::open_component(
            &litci,
            &home_path.join(".litci"),
            "metrics".as_ref(),
            create,
        )
    }
}

fn explicit_parent_and_name(path: &Path) -> Result<(&Path, &std::ffi::OsStr), MetricsError> {
    let name = path
        .file_name()
        .ok_or_else(|| MetricsError::InvalidPathComponent {
            path: path.to_path_buf(),
        })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok((parent, name))
}
