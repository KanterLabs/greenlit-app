//! One-lookup establishment of the physical workspace root.

use std::io;
use std::path::{Component, Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};

use super::errno_to_io;

const ROOT_RESOLVE_FLAGS: ResolveFlags =
    ResolveFlags::NO_SYMLINKS.union(ResolveFlags::NO_MAGICLINKS);

pub(super) struct EstablishedRoot {
    pub(super) path: PathBuf,
    pub(super) fd: Option<OwnedFd>,
    pub(super) error: Option<RootOpenError>,
}

#[derive(Debug)]
pub(super) struct RootOpenError {
    kind: io::ErrorKind,
    message: String,
}

pub(super) fn establish(requested: PathBuf) -> EstablishedRoot {
    let normalized = absolutize_normalized(requested.clone());
    let path = normalized.as_ref().map_or(requested, Clone::clone);
    let opened = normalized.and_then(|path| {
        openat2(
            CWD,
            path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ROOT_RESOLVE_FLAGS,
        )
        .map_err(root_errno_to_io)
    });
    match opened {
        Ok(fd) => EstablishedRoot {
            path,
            fd: Some(fd),
            error: None,
        },
        Err(error) => EstablishedRoot {
            path,
            fd: None,
            error: Some(RootOpenError::from_io(error)),
        },
    }
}

impl RootOpenError {
    fn from_io(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    pub(super) fn to_io(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

fn absolutize_normalized(path: PathBuf) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    if normalized.is_absolute() {
        Ok(normalized)
    } else {
        Err(io::Error::other(
            "couldn't establish an absolute hashFiles workspace path; use an absolute physical directory path",
        ))
    }
}

fn root_errno_to_io(error: rustix::io::Errno) -> io::Error {
    if error == rustix::io::Errno::LOOP {
        return io::Error::new(
            io::ErrorKind::InvalidInput,
            "the hashFiles workspace path contains a symbolic link; use the workspace's physical directory path",
        );
    }
    if error == rustix::io::Errno::NOTDIR {
        return io::Error::new(
            io::ErrorKind::InvalidInput,
            "the hashFiles workspace path is not a physical directory; use the workspace's physical directory path",
        );
    }
    errno_to_io(error)
}
