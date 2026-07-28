//! Descriptor-relative creation inside one frozen source tree.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags, chmod, fchmod, mkdirat, open, openat, symlinkat};
use rustix::io::Errno;

use super::{SourceSnapshotError, io_error};

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

pub(super) struct PrivateTree {
    root: File,
    path: PathBuf,
}

pub(super) struct PrivateTarget {
    parent: File,
    name: OsString,
    path: PathBuf,
}

impl PrivateTree {
    pub(super) fn open(path: &Path) -> Result<Self, SourceSnapshotError> {
        let root = open(
            path,
            OFlags::RDONLY
                | OFlags::DIRECTORY
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| io_error(path, error))?;
        validate_directory(path, &root)?;
        Ok(Self {
            root,
            path: path.to_path_buf(),
        })
    }

    pub(super) fn target(&self, relative: &str) -> Result<PrivateTarget, SourceSnapshotError> {
        let relative_path = Path::new(relative);
        let name = relative_path
            .file_name()
            .ok_or_else(|| io_error(relative_path, "source path has no file name"))?
            .to_os_string();
        let mut parent = self
            .root
            .try_clone()
            .map_err(|error| io_error(&self.path, error))?;
        let mut parent_path = self.path.clone();
        if let Some(components) = relative_path.parent() {
            for component in components.components() {
                let Component::Normal(component) = component else {
                    return Err(io_error(
                        relative_path,
                        "source path contains a non-normal component",
                    ));
                };
                parent = create_or_open_directory(&parent, &parent_path, component)?;
                parent_path.push(component);
            }
        }
        Ok(PrivateTarget {
            parent,
            name,
            path: self.path.join(relative_path),
        })
    }
}

impl PrivateTarget {
    pub(super) fn create_file(&self) -> Result<File, SourceSnapshotError> {
        let file = openat(
            &self.parent,
            &self.name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map(File::from)
        .map_err(|error| io_error(&self.path, error))?;
        let metadata = file
            .metadata()
            .map_err(|error| io_error(&self.path, error))?;
        validate_owner(&self.path, &metadata)?;
        let mode = metadata.mode() & 0o7777;
        if !metadata.is_file() || metadata.nlink() != 1 || mode & !FILE_MODE != 0 {
            return Err(io_error(
                &self.path,
                format!(
                    "new frozen-source file has unsafe type, mode 0{mode:03o}, or link count {}",
                    metadata.nlink()
                ),
            ));
        }
        fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(|error| io_error(&self.path, error))?;
        validate_file(&self.path, &file)?;
        Ok(file)
    }

    pub(super) fn create_symlink(&self, target: &Path) -> Result<(), SourceSnapshotError> {
        symlinkat(target, &self.parent, &self.name).map_err(|error| io_error(&self.path, error))
    }
}

fn create_or_open_directory(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
) -> Result<File, SourceSnapshotError> {
    let path = parent_path.join(name);
    match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => open_new_directory(parent, &path, name),
        Err(Errno::EXIST) => open_existing_directory(parent, &path, name),
        Err(error) => Err(io_error(&path, error)),
    }
}

fn open_new_directory(
    parent: &File,
    path: &Path,
    name: &OsStr,
) -> Result<File, SourceSnapshotError> {
    let inspected = openat(
        parent,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error(path, error))?;
    let before = inspected
        .metadata()
        .map_err(|error| io_error(path, error))?;
    validate_owner(path, &before)?;
    let mode = before.mode() & 0o7777;
    if !before.is_dir() || mode & !0o2700 != 0 {
        return Err(io_error(
            path,
            format!("new frozen-source directory has unsafe type or mode 0{mode:03o}"),
        ));
    }
    chmod_descriptor(&inspected, path, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
    let directory = open_existing_directory(parent, path, name)?;
    let after = directory
        .metadata()
        .map_err(|error| io_error(path, error))?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(io_error(
            path,
            "new frozen-source directory changed while its mode was normalized",
        ));
    }
    Ok(directory)
}

fn open_existing_directory(
    parent: &File,
    path: &Path,
    name: &OsStr,
) -> Result<File, SourceSnapshotError> {
    let directory = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error(path, error))?;
    validate_directory(path, &directory)?;
    Ok(directory)
}

fn validate_directory(path: &Path, directory: &File) -> Result<(), SourceSnapshotError> {
    let metadata = directory
        .metadata()
        .map_err(|error| io_error(path, error))?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir() || mode != DIRECTORY_MODE {
        return Err(io_error(
            path,
            format!("frozen-source directory has unsafe type or mode 0{mode:03o}"),
        ));
    }
    Ok(())
}

fn validate_file(path: &Path, file: &File) -> Result<(), SourceSnapshotError> {
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.nlink() != 1 || mode != FILE_MODE {
        return Err(io_error(
            path,
            format!(
                "frozen-source file has unsafe type, mode 0{mode:03o}, or link count {}",
                metadata.nlink()
            ),
        ));
    }
    Ok(())
}

fn validate_owner(path: &Path, metadata: &std::fs::Metadata) -> Result<(), SourceSnapshotError> {
    let current_uid = rustix::process::getuid().as_raw();
    if metadata.uid() != current_uid {
        return Err(io_error(
            path,
            format!(
                "frozen-source path is owned by uid {}, not current uid {current_uid}",
                metadata.uid()
            ),
        ));
    }
    Ok(())
}

fn chmod_descriptor(file: &File, path: &Path, mode: Mode) -> Result<(), SourceSnapshotError> {
    let descriptor = format!("/proc/self/fd/{}", file.as_raw_fd());
    chmod(descriptor, mode).map_err(|error| {
        io_error(
            path,
            format!(
                "could not normalize the new frozen-source directory through its stable descriptor: {error}"
            ),
        )
    })
}
