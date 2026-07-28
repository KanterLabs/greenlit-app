//! Exact private directory creation below the user-local Greenlit state root.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use rustix::fs::{Mode, OFlags, chmod, mkdirat, open, openat};
use rustix::io::Errno;

pub(super) fn ensure_directory(state_root: &Path, relative: &Path) -> std::io::Result<File> {
    let mut directory = open(
        state_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    validate_directory(state_root, &directory)?;
    let mut directory_path = state_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(unsafe_inode(
                relative,
                "private state path contains a non-normal component",
            ));
        };
        directory = create_or_open(&directory, &directory_path, name)?;
        directory_path.push(name);
    }
    Ok(directory)
}

fn create_or_open(
    parent: &File,
    parent_path: &Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<File> {
    let path = parent_path.join(name);
    match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => open_new(parent, &path, name),
        Err(Errno::EXIST) => open_existing(parent, &path, name),
        Err(error) => Err(error.into()),
    }
}

fn open_new(parent: &File, path: &Path, name: &std::ffi::OsStr) -> std::io::Result<File> {
    let inspected = openat(
        parent,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    let before = inspected.metadata()?;
    validate_owner(path, &before)?;
    let mode = before.mode() & 0o7777;
    if !before.is_dir() || mode & !0o2700 != 0 {
        return Err(unsafe_inode(
            path,
            format!("new directory has unsafe type or mode 0{mode:03o}"),
        ));
    }
    let descriptor = format!("/proc/self/fd/{}", inspected.as_raw_fd());
    chmod(descriptor, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(std::io::Error::from)?;
    let directory = open_existing(parent, path, name)?;
    let after = directory.metadata()?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(unsafe_inode(
            path,
            "new directory changed while its mode was normalized",
        ));
    }
    Ok(directory)
}

fn open_existing(parent: &File, path: &Path, name: &std::ffi::OsStr) -> std::io::Result<File> {
    let directory = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    validate_directory(path, &directory)?;
    Ok(directory)
}

fn validate_directory(path: &Path, directory: &File) -> std::io::Result<()> {
    let metadata = directory.metadata()?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir() || mode != 0o700 {
        return Err(unsafe_inode(
            path,
            format!("directory has unsafe type or mode 0{mode:03o}"),
        ));
    }
    Ok(())
}

fn validate_owner(path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<()> {
    let current_uid = rustix::process::getuid().as_raw();
    if metadata.uid() == current_uid {
        Ok(())
    } else {
        Err(unsafe_inode(
            path,
            format!(
                "path is owned by uid {}, not current uid {current_uid}",
                metadata.uid()
            ),
        ))
    }
}

fn unsafe_inode(path: &Path, detail: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "refused unsafe Greenlit state path {}: {detail}",
            path.display()
        ),
    )
}
