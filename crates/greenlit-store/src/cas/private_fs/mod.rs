//! Descriptor-relative private inode creation for the persistent CAS.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, chmod, mkdirat, openat, openat2};
use rustix::io::Errno;

use super::{CasError, io_error};

mod file;

use file::{create_new_file_at, open_existing_file_at};

const DIRECTORY_MODE: u32 = 0o700;

#[derive(Debug)]
pub(super) struct PrivateStore {
    root: File,
    path: PathBuf,
}

impl PrivateStore {
    pub(super) fn open(path: &Path) -> Result<Self, CasError> {
        let root = match open_existing_directory_path(path) {
            Ok(directory) => directory,
            Err(CasError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                create_root(path)?
            }
            Err(error) => return Err(error),
        };
        validate_directory(path, &root)?;
        Ok(Self {
            root,
            path: path.to_path_buf(),
        })
    }

    pub(super) fn ensure_directory(&self, relative: &Path) -> Result<File, CasError> {
        let mut directory = self
            .root
            .try_clone()
            .map_err(|error| io_error(&self.path, error))?;
        let mut directory_path = self.path.clone();
        for component in normal_components(relative)? {
            directory = create_or_open_directory(&directory, &directory_path, &component)?;
            directory_path.push(component);
        }
        Ok(directory)
    }

    pub(super) fn ensure_file(&self, relative: &Path) -> Result<File, CasError> {
        let (parent, path, name) = self.file_parent(relative)?;
        loop {
            match open_existing_file_at(&parent, &path, &name, OFlags::RDWR) {
                Ok(file) => return Ok(file),
                Err(CasError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    match create_new_file_at(&parent, &path, &name) {
                        Ok(file) => return Ok(file),
                        Err(CasError::Io { source, .. })
                            if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn create_new_file(&self, relative: &Path) -> Result<File, CasError> {
        let (parent, path, name) = self.file_parent(relative)?;
        create_new_file_at(&parent, &path, &name)
    }

    pub(super) fn create_exclusive_file(&self, relative: &Path) -> Result<Option<File>, CasError> {
        let (parent, path, name) = self.file_parent(relative)?;
        loop {
            match create_new_file_at(&parent, &path, &name) {
                Ok(file) => return Ok(Some(file)),
                Err(CasError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    match open_existing_file_at(&parent, &path, &name, OFlags::RDONLY) {
                        Ok(_) => return Ok(None),
                        Err(CasError::Io { source, .. })
                            if source.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn open_file(&self, relative: &Path, access: OFlags) -> Result<File, CasError> {
        let (parent, path, name) = self.file_parent(relative)?;
        open_existing_file_at(&parent, &path, &name, access)
    }

    fn file_parent(&self, relative: &Path) -> Result<(File, PathBuf, OsString), CasError> {
        let mut components = normal_components(relative)?;
        let name = components.pop().ok_or_else(|| {
            io_error(
                &self.path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "content-store file path has no file name",
                ),
            )
        })?;
        let parent_relative = components.iter().collect::<PathBuf>();
        let parent = self.ensure_directory(&parent_relative)?;
        Ok((parent, self.path.join(relative), name))
    }
}

fn create_root(path: &Path) -> Result<File, CasError> {
    let parent_path = path.parent().ok_or_else(|| {
        io_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "content-store root has no parent directory",
            ),
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        io_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "content-store root has no final path component",
            ),
        )
    })?;
    let parent = open_or_create_private_parent(parent_path)?;
    match mkdirat(&parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => open_new_directory(&parent, path, name),
        Err(Errno::EXIST) => open_existing_directory_at(&parent, path, name),
        Err(error) => Err(io_error(path, error.into())),
    }
}

fn open_or_create_private_parent(path: &Path) -> Result<File, CasError> {
    match open_existing_directory_path(path) {
        Ok(directory) => {
            let metadata = directory
                .metadata()
                .map_err(|error| io_error(path, error))?;
            validate_owner(path, &metadata)?;
            Ok(directory)
        }
        Err(CasError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            create_missing_private_path(path)
        }
        Err(error) => Err(error),
    }
}

fn create_missing_private_path(path: &Path) -> Result<File, CasError> {
    let mut missing = Vec::<OsString>::new();
    let mut existing_path = path;
    let existing = loop {
        match open_existing_directory_path(existing_path) {
            Ok(directory) => break directory,
            Err(CasError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                let name = existing_path.file_name().ok_or_else(|| {
                    io_error(
                        path,
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "content-store path has no existing ancestor",
                        ),
                    )
                })?;
                missing.push(name.to_os_string());
                existing_path = existing_path.parent().ok_or_else(|| {
                    io_error(
                        path,
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "content-store path has no existing ancestor",
                        ),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    };
    validate_owner(
        existing_path,
        &existing
            .metadata()
            .map_err(|error| io_error(existing_path, error))?,
    )?;

    let mut directory = existing;
    let mut directory_path = existing_path.to_path_buf();
    for name in missing.into_iter().rev() {
        directory = match mkdirat(&directory, &name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
            Ok(()) => open_new_directory(&directory, &directory_path.join(&name), &name)?,
            Err(Errno::EXIST) => {
                open_existing_directory_at(&directory, &directory_path.join(&name), &name)?
            }
            Err(error) => return Err(io_error(&directory_path.join(&name), error.into())),
        };
        directory_path.push(name);
    }
    Ok(directory)
}

fn create_or_open_directory(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
) -> Result<File, CasError> {
    let path = parent_path.join(name);
    match mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => open_new_directory(parent, &path, name),
        Err(Errno::EXIST) => open_existing_directory_at(parent, &path, name),
        Err(error) => Err(io_error(&path, error.into())),
    }
}

fn open_new_directory(parent: &File, path: &Path, name: &OsStr) -> Result<File, CasError> {
    let inspected = openat(
        parent,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error(path, error.into()))?;
    let before = inspected
        .metadata()
        .map_err(|error| io_error(path, error))?;
    validate_owner(path, &before)?;
    let mode = before.mode() & 0o7777;
    if !before.is_dir() || mode & !0o2700 != 0 {
        return Err(unsafe_path_error(
            path,
            format!("new directory has unsafe type or mode 0{mode:03o}"),
        ));
    }
    chmod_descriptor(&inspected, path)?;
    let directory = open_existing_directory_at(parent, path, name)?;
    let after = directory
        .metadata()
        .map_err(|error| io_error(path, error))?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(unsafe_path_error(
            path,
            "new directory changed while its private mode was normalized",
        ));
    }
    Ok(directory)
}

fn open_existing_directory_at(parent: &File, path: &Path, name: &OsStr) -> Result<File, CasError> {
    let directory = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error(path, error.into()))?;
    validate_directory(path, &directory)?;
    Ok(directory)
}

fn open_existing_directory_path(path: &Path) -> Result<File, CasError> {
    openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
    )
    .map(File::from)
    .map_err(|error| io_error(path, error.into()))
}

fn validate_directory(path: &Path, directory: &File) -> Result<(), CasError> {
    let metadata = directory
        .metadata()
        .map_err(|error| io_error(path, error))?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir() || mode != DIRECTORY_MODE {
        return Err(unsafe_path_error(
            path,
            format!("directory has unsafe type or mode 0{mode:03o}"),
        ));
    }
    Ok(())
}

fn validate_owner(path: &Path, metadata: &std::fs::Metadata) -> Result<(), CasError> {
    let current_uid = rustix::process::getuid().as_raw();
    if metadata.uid() == current_uid {
        Ok(())
    } else {
        Err(unsafe_path_error(
            path,
            format!(
                "path is owned by uid {}, not current uid {current_uid}",
                metadata.uid()
            ),
        ))
    }
}

fn chmod_descriptor(directory: &File, path: &Path) -> Result<(), CasError> {
    let descriptor = format!("/proc/self/fd/{}", directory.as_raw_fd());
    chmod(descriptor, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(|error| {
        io_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "could not normalize the new directory through its stable descriptor: {error}"
                ),
            ),
        )
    })
}

fn normal_components(path: &Path) -> Result<Vec<OsString>, CasError> {
    path.components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_os_string()),
            _ => Err(io_error(
                path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "content-store relative path contains a non-normal component",
                ),
            )),
        })
        .collect()
}

fn unsafe_path_error(path: &Path, detail: impl std::fmt::Display) -> CasError {
    io_error(
        path,
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refused unsafe existing content-store path: {detail}"),
        ),
    )
}
