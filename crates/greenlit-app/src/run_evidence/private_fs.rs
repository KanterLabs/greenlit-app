//! Descriptor-relative exact private creation for retained run evidence.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, chmod, fchmod, mkdirat, open, openat};
use rustix::io::Errno;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

pub(crate) fn create_private_artifact(
    directory: &Path,
    name: &OsStr,
    append: bool,
) -> anyhow::Result<File> {
    let handle = open(
        directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| unsafe_path_error(directory, error))?;
    validate_private_directory(directory, &handle)?;
    create_private_file_at(&handle, &directory.join(name), name, append)
}

pub(super) fn prepare_runs_directory(home: &Path) -> anyhow::Result<(PathBuf, File, File)> {
    let home_handle = open(
        home,
        OFlags::RDONLY
            | OFlags::DIRECTORY
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        anyhow::anyhow!(
            "could not open HOME for run evidence at {}: {error}\n  fix: set HOME to an absolute writable directory owned by the current user, then retry",
            home.display()
        )
    })?;
    validate_current_owner(home, &home_handle.metadata().map_err(|error| {
        anyhow::anyhow!(
            "could not inspect HOME for run evidence at {}: {error}\n  fix: set HOME to an absolute writable directory owned by the current user, then retry",
            home.display()
        )
    })?)?;
    let litci = create_or_open_private_directory(&home_handle, home, ".litci")?;
    let litci_path = home.join(".litci");
    let runs = create_or_open_private_directory(&litci, &litci_path, "runs")?;
    Ok((litci_path.join("runs"), runs, home_handle))
}

fn create_or_open_private_directory(
    parent: &File,
    parent_path: &Path,
    name: &str,
) -> anyhow::Result<File> {
    match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => open_new_private_directory_at(parent, parent_path, name),
        Err(Errno::EXIST) => open_private_directory_at(parent, parent_path, name),
        Err(error) => Err(evidence_write_error(&parent_path.join(name), error)),
    }
}

pub(super) fn create_new_private_directory(
    parent: &File,
    parent_path: &Path,
    name: &str,
) -> anyhow::Result<File> {
    let path = parent_path.join(name);
    match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => open_new_private_directory_at(parent, parent_path, name),
        Err(Errno::EXIST) => {
            let existing = open_private_directory_at(parent, parent_path, name)?;
            drop(existing);
            Err(evidence_write_error(
                &path,
                "a directory already exists at this write-once evidence path",
            ))
        }
        Err(error) => Err(evidence_write_error(&path, error)),
    }
}

pub(super) fn open_new_private_directory_at(
    parent: &File,
    parent_path: &Path,
    name: &str,
) -> anyhow::Result<File> {
    let path = parent_path.join(name);
    let inspected = openat(
        parent,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| unsafe_path_error(&path, error))?;
    let metadata = inspected
        .metadata()
        .map_err(|error| evidence_write_error(&path, error))?;
    if !metadata.is_dir() {
        return Err(unsafe_path_error(&path, "path is not a directory"));
    }
    validate_current_owner(&path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if mode & !0o2700 != 0 {
        return Err(unsafe_path_error(
            &path,
            format!("new private directory has unexpected mode 0{mode:03o}"),
        ));
    }
    // Linux can inherit SGID despite the requested mode, and a restrictive
    // umask can remove owner access. Normalize the verified, still-empty
    // inode before traversing it or creating a retained child.
    chmod_descriptor(&inspected, &path, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
    let file = open_directory_at(parent, &path, name)?;
    let reopened = file
        .metadata()
        .map_err(|error| evidence_write_error(&path, error))?;
    if (reopened.dev(), reopened.ino()) != (metadata.dev(), metadata.ino()) {
        return Err(unsafe_path_error(
            &path,
            "new private directory changed while its mode was normalized",
        ));
    }
    validate_private_directory(&path, &file)?;
    Ok(file)
}

pub(super) fn open_private_directory_at(
    parent: &File,
    parent_path: &Path,
    name: &str,
) -> anyhow::Result<File> {
    let path = parent_path.join(name);
    let file = open_directory_at(parent, &path, name)?;
    validate_private_directory(&path, &file)?;
    Ok(file)
}

fn open_directory_at(parent: &File, path: &Path, name: &str) -> anyhow::Result<File> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| unsafe_path_error(path, error))
}

pub(super) fn create_private_file_at(
    parent: &File,
    path: &Path,
    name: &OsStr,
    append: bool,
) -> anyhow::Result<File> {
    let mut flags =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    if append {
        flags |= OFlags::APPEND;
    }
    let file = match openat(parent, name, flags, Mode::RUSR | Mode::WUSR) {
        Ok(fd) => File::from(fd),
        Err(Errno::EXIST) => {
            reject_unsafe_existing_file(parent, path, name)?;
            return Err(evidence_write_error(
                path,
                "a file already exists at this write-once evidence path",
            ));
        }
        Err(error) => return Err(evidence_write_error(path, error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| evidence_write_error(path, error))?;
    if !metadata.is_file() {
        return Err(unsafe_path_error(path, "new path is not a regular file"));
    }
    validate_current_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if mode & !PRIVATE_FILE_MODE != 0 || metadata.nlink() != 1 {
        return Err(unsafe_path_error(
            path,
            format!(
                "new private file has unexpected mode 0{mode:03o} or link count {}",
                metadata.nlink()
            ),
        ));
    }
    // Normalize the new descriptor before its first byte is written.
    fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(|error| evidence_write_error(path, error))?;
    validate_private_file(path, &file)?;
    Ok(file)
}

pub(super) fn open_private_file_at(
    parent: &File,
    path: &Path,
    name: &OsStr,
    append: bool,
) -> anyhow::Result<File> {
    let mut flags = OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    if append {
        flags |= OFlags::APPEND;
    }
    let file = openat(parent, name, flags, Mode::empty())
        .map(File::from)
        .map_err(|error| unsafe_path_error(path, error))?;
    validate_private_file(path, &file)?;
    Ok(file)
}

pub(super) fn reject_unsafe_existing_file(
    parent: &File,
    path: &Path,
    name: &OsStr,
) -> anyhow::Result<()> {
    match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => validate_private_file(path, &File::from(fd)),
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(unsafe_path_error(path, error)),
    }
}

fn validate_private_directory(path: &Path, directory: &File) -> anyhow::Result<()> {
    let metadata = directory
        .metadata()
        .map_err(|error| evidence_write_error(path, error))?;
    if !metadata.is_dir() {
        return Err(unsafe_path_error(path, "path is not a directory"));
    }
    validate_private_metadata(path, &metadata, PRIVATE_DIRECTORY_MODE)
}

fn validate_private_file(path: &Path, file: &File) -> anyhow::Result<()> {
    let metadata = file
        .metadata()
        .map_err(|error| evidence_write_error(path, error))?;
    if !metadata.is_file() {
        return Err(unsafe_path_error(path, "path is not a regular file"));
    }
    if metadata.nlink() != 1 {
        return Err(unsafe_path_error(
            path,
            format!(
                "private run evidence file has link count {} instead of 1",
                metadata.nlink()
            ),
        ));
    }
    validate_private_metadata(path, &metadata, PRIVATE_FILE_MODE)
}

fn validate_private_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    required_mode: u32,
) -> anyhow::Result<()> {
    validate_current_owner(path, metadata)?;
    let mode = metadata.mode() & 0o7777;
    if mode != required_mode {
        anyhow::bail!(
            "refused unsafe run evidence path {} because its mode is 0{mode:03o}\n  fix: change its mode to 0{required_mode:03o} and ensure it is owned by the current user, then retry",
            path.display()
        );
    }
    Ok(())
}

fn validate_current_owner(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    let current_uid = rustix::process::getuid().as_raw();
    if metadata.uid() == current_uid {
        return Ok(());
    }
    anyhow::bail!(
        "refused unsafe run evidence path {} because it is owned by uid {}, not the current uid {current_uid}\n  fix: move the path aside or make it private and owned by the current user, then retry",
        path.display(),
        metadata.uid()
    )
}

fn chmod_descriptor(file: &File, path: &Path, mode: Mode) -> anyhow::Result<()> {
    let descriptor = format!("/proc/self/fd/{}", file.as_raw_fd());
    chmod(descriptor, mode).map_err(|error| {
        anyhow::anyhow!(
            "could not normalize the new private run evidence path {} through its stable descriptor: {error}\n  fix: mount procfs at /proc and ensure HOME is writable, then retry",
            path.display()
        )
    })
}

pub(super) fn unsafe_path_error(path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "refused unsafe run evidence path {}: {error}\n  fix: move the path aside or make it private and owned by the current user, then retry",
        path.display()
    )
}

pub(super) fn evidence_write_error(path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "could not persist run evidence at {}: {error}\n  fix: ensure HOME has free space and is writable, then retry",
        path.display()
    )
}
