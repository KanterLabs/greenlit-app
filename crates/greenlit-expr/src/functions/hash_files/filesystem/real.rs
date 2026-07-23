//! Linux production filesystem confined by a held workspace descriptor.

mod directory;
mod node;
mod root;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{FileType, Mode, OFlags, ResolveFlags, Stat, openat2};
use sha2::{Digest, Sha256};

use self::directory::RealOpenDir;
use self::node::{DirNode, Resolved, components};
use self::root::{RootOpenError, establish};
use super::{EntryKind, HashFilesFs, OpenedDir};

const RESOLVE_FLAGS: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);

/// The production [`HashFilesFs`], confined beneath one held workspace
/// descriptor for its full lifetime.
///
/// Every access — the workspace root, a literal search root, or a child of
/// an already-open directory — is opened relative to a held ancestor
/// descriptor with `openat2` and no-follow inspection; see `filesystem/
/// real/node.rs` for the resolver and its rationale. Root establishment and
/// `$HOME` canonicalization are both lazy (behind `OnceLock`) so that
/// neither blocking host filesystem call happens outside the caller's
/// supervised, deadline-checked worker: `RealFs::new` performs no I/O, and
/// every trait method that first touches the filesystem is only ever
/// called from inside `hash_files_worker` (finding 2 of the hashFiles
/// hardening review).
#[derive(Debug)]
pub struct RealFs {
    requested_root: PathBuf,
    established: OnceLock<Established>,
    home: OnceLock<Option<PathBuf>>,
}

struct Established {
    path: PathBuf,
    root: Option<Arc<DirNode>>,
    error: Option<RootOpenError>,
}

impl std::fmt::Debug for Established {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Established")
            .field("path", &self.path)
            .field("established", &self.root.is_some())
            .finish()
    }
}

#[derive(Clone, Copy)]
enum Access {
    Identity,
    ReadDirectory,
    ReadFile,
}

impl RealFs {
    /// Builds a real filesystem rooted at `root` (GitHub's
    /// `GITHUB_WORKSPACE` equivalent). This constructor performs no I/O:
    /// the root is only opened, and `$HOME` only canonicalized, on first
    /// actual use (see the type-level documentation).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            requested_root: root.into(),
            established: OnceLock::new(),
            home: OnceLock::new(),
        }
    }

    fn established(&self) -> &Established {
        self.established.get_or_init(|| {
            let established = establish(self.requested_root.clone());
            Established {
                path: established.path,
                root: established.fd.map(DirNode::root),
                error: established.error,
            }
        })
    }

    fn root_path(&self) -> &Path {
        &self.established().path
    }

    fn root_node(&self) -> io::Result<&Arc<DirNode>> {
        let established = self.established();
        established.root.as_ref().ok_or_else(|| {
            established.error.as_ref().map_or_else(
                || io::Error::other("hashFiles workspace descriptor is unavailable"),
                RootOpenError::to_io,
            )
        })
    }

    fn pending_for(&self, path: &Path) -> io::Result<VecDeque<OsString>> {
        let relative = path
            .strip_prefix(self.root_path())
            .map_err(|_| outside_workspace(path))?;
        components(relative, path)
    }

    /// Opens one entry point: the workspace root, `$HOME`, or a literal
    /// search root. Called at most once per entry point — see
    /// `HashFilesFs::open_dir`'s contract.
    fn open_entry_point(
        &self,
        path: &Path,
        follow_final: bool,
        access: Access,
    ) -> io::Result<Resolved> {
        let root = self.root_node()?;
        let pending = self.pending_for(path)?;
        node::resolve(
            root,
            self.root_path(),
            root,
            pending,
            follow_final,
            access,
            path,
        )
    }

    /// Opens `name`, relative to the already-held `parent`, never by
    /// re-deriving `lexical_path` and re-resolving it from the workspace
    /// root (findings 1 and 6 of the hashFiles hardening review).
    fn open_relative(
        &self,
        parent: &Arc<DirNode>,
        name: &str,
        lexical_path: &Path,
        follow: bool,
        access: Access,
    ) -> io::Result<Resolved> {
        if name.contains('/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hashFiles child name contained a path separator",
            ));
        }
        let root = self.root_node()?;
        let mut pending = VecDeque::new();
        pending.push_back(OsString::from(name));
        node::resolve(
            root,
            self.root_path(),
            parent,
            pending,
            follow,
            access,
            lexical_path,
        )
    }
}

impl HashFilesFs for RealFs {
    fn workspace_root(&self) -> &Path {
        self.root_path()
    }

    fn home_dir(&self) -> Option<&Path> {
        self.home
            .get_or_init(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .and_then(|path| std::fs::canonicalize(path).ok())
            })
            .as_deref()
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        let resolved = self.open_entry_point(path, true, Access::ReadDirectory)?;
        wrap_dir(self, resolved, path)
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        let resolved = self.open_entry_point(path, false, Access::Identity)?;
        Ok(entry_kind(FileType::from_raw_mode(
            resolved.target.stat.st_mode,
        )))
    }

    fn hash_file_sha256(
        &self,
        path: &Path,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        check_timeout()?;
        let resolved = self.open_entry_point(path, true, Access::ReadFile)?;
        check_timeout()?;
        hash_from_fd(resolved.target.fd, check_timeout)
    }
}

fn wrap_dir<'a>(fs: &'a RealFs, resolved: Resolved, path: &Path) -> io::Result<OpenedDir<'a>> {
    let identity = descriptor_identity(&resolved.target.stat);
    let node = resolved.directory.ok_or_else(|| {
        io::Error::other(format!(
            "hashFiles directory resolution did not identify its own object: {}",
            path.display()
        ))
    })?;
    let directory = rustix::fs::Dir::new(resolved.target.fd).map_err(errno_to_io)?;
    Ok(OpenedDir::new(
        identity,
        Box::new(RealOpenDir::new(fs, node, directory)),
    ))
}

fn hash_from_fd(
    fd: OwnedFd,
    check_timeout: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<[u8; 32]> {
    let mut file = std::fs::File::from(fd);
    let mut buffer = [0_u8; 64 * 1024];
    let mut hash = Sha256::new();
    loop {
        check_timeout()?;
        let read = file.read(&mut buffer)?;
        check_timeout()?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    check_timeout()?;
    Ok(hash.finalize().into())
}

fn open_component<Fd: AsFd>(
    parent: Fd,
    name: impl rustix::path::Arg,
    flags: OFlags,
) -> io::Result<OwnedFd> {
    openat2(parent, name, flags, Mode::empty(), RESOLVE_FLAGS).map_err(errno_to_io)
}

/// Flags for the magic-link reopen in `node.rs::finalize`. Deliberately
/// `O_NOFOLLOW`-free: `/proc/self/fd/<n>` is itself a symlink and must be
/// followed to reach the descriptor it names — that is the whole point of
/// reopening through it instead of by the original (attacker-influenced)
/// pathname. The type was already validated with `O_NOFOLLOW` at the
/// `O_PATH` probe; this open only ever names a specific fd this process
/// already holds.
fn access_flags(access: Access) -> OFlags {
    match access {
        Access::Identity => OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Access::ReadDirectory => {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NONBLOCK | OFlags::CLOEXEC
        }
        Access::ReadFile => OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
    }
}

fn descriptor_identity(stat: &Stat) -> PathBuf {
    PathBuf::from(format!(
        "/.greenlit-hash-files/{:016x}/{:016x}",
        stat.st_dev, stat.st_ino
    ))
}

fn entry_kind(file_type: FileType) -> EntryKind {
    match file_type {
        FileType::RegularFile => EntryKind::File,
        FileType::Directory => EntryKind::Dir,
        FileType::Symlink => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

fn outside_workspace(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("path escapes the hashFiles workspace: {}", path.display()),
    )
}

fn errno_to_io(error: rustix::io::Errno) -> io::Error {
    if error == rustix::io::Errno::NOSYS {
        return io::Error::new(
            io::ErrorKind::Unsupported,
            "contained hashFiles access requires Linux 5.6 or newer; upgrade the host kernel",
        );
    }
    io::Error::from_raw_os_error(error.raw_os_error())
}
