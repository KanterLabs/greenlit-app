//! The held-ancestor directory chain and the single component-at-a-time
//! resolver every access — entry point or child — walks through.
//!
//! Findings 1/6/7 of the hardening review: the previous implementation
//! re-resolved a full lexical path from the workspace root on every
//! directory or file access, which (a) let a rename of an already-inspected
//! ancestor substitute a different object before the next access reached
//! it, and (b) reopened a final file by name a second time after
//! validating its type, racing a replacement. `DirNode` instead keeps every
//! currently-open ancestor's descriptor alive for as long as the traversal
//! is anywhere beneath it (in an `Arc` chain mirroring the DFS stack), so a
//! child is always opened via `openat2(ancestor_fd, single_component, …)`
//! relative to the object that was actually inspected — never by
//! re-deriving and re-walking a path string. `Arc` (not `Rc`) because the
//! root node is shared across every worker thread this `RealFs` ever
//! serves (see `filesystem.rs`'s thread-safety note). Every `DirNode::fd`
//! is `O_PATH`-only (navigation, never reads); a readable handle is minted
//! fresh, from that exact descriptor's own `/proc/self/fd` entry, only at
//! the point something is actually enumerated or hashed.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fd::{AsRawFd, OwnedFd};
use rustix::fs::{FileType, Mode, OFlags, Stat, fstat, readlinkat};
use rustix::io::fcntl_dupfd_cloexec;

use super::{Access, access_flags, errno_to_io, open_component, outside_workspace};

const MAX_SYMLINKS: usize = 40;

/// One held ancestor directory descriptor. The chain terminates at the
/// pinned workspace root, whose `parent` is `None`.
pub(super) struct DirNode {
    pub(super) fd: OwnedFd,
    parent: Option<Arc<DirNode>>,
}

impl DirNode {
    pub(super) fn root(fd: OwnedFd) -> Arc<Self> {
        Arc::new(Self { fd, parent: None })
    }
}

pub(super) struct OpenedTarget {
    pub(super) fd: OwnedFd,
    pub(super) stat: Stat,
}

/// The outcome of [`resolve`]: a readable target descriptor, plus — only
/// when that target is itself a directory — the ancestor node for it,
/// ready to become the parent of further held-relative resolution.
pub(super) struct Resolved {
    pub(super) target: OpenedTarget,
    pub(super) directory: Option<Arc<DirNode>>,
}

/// Resolves `pending` (one or more path components, e.g. a single child
/// name, or more after a symbolic link target expands it) relative to
/// `start`, which must already be a held, inspected directory. `..` is
/// handled by walking `start`'s own `parent` chain rather than by asking
/// the kernel to resolve it, so it can never climb above the node the
/// traversal actually pinned as its root — the same containment guarantee
/// `openat2`'s `RESOLVE_BENEATH` gives a single call, but now honored
/// across the whole multi-step traversal instead of only within one.
pub(super) fn resolve(
    root: &Arc<DirNode>,
    root_path: &Path,
    start: &Arc<DirNode>,
    mut pending: VecDeque<OsString>,
    follow_final: bool,
    access: Access,
    lexical_path: &Path,
) -> io::Result<Resolved> {
    let mut current = Arc::clone(start);
    let mut symlinks = 0_usize;

    loop {
        let Some(component) = pending.pop_front() else {
            // Resolving `start` itself (e.g. an entry point whose lexical
            // path was exactly the already-open directory): duplicate its
            // descriptor rather than moving it, since `current` here is
            // only ever borrowed from a caller-held `Arc`.
            let inspected = fcntl_dupfd_cloexec(&current.fd, 0).map_err(errno_to_io)?;
            let inspected_stat = fstat(&inspected).map_err(errno_to_io)?;
            let parent = current.parent.clone();
            return finalize(inspected, inspected_stat, access, lexical_path, parent);
        };
        if component == OsStr::new(".") {
            continue;
        }
        if component == OsStr::new("..") {
            match &current.parent {
                Some(parent) => {
                    current = Arc::clone(parent);
                    continue;
                }
                None => return Err(outside_workspace(lexical_path)),
            }
        }

        let inspected = open_component(
            &current.fd,
            &component,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        )?;
        let inspected_stat = fstat(&inspected).map_err(errno_to_io)?;
        let file_type = FileType::from_raw_mode(inspected_stat.st_mode);
        let is_final = pending.is_empty();

        if file_type == FileType::Symlink && (follow_final || !is_final) {
            symlinks = symlinks.saturating_add(1);
            if symlinks > MAX_SYMLINKS {
                return Err(io::Error::from_raw_os_error(
                    rustix::io::Errno::LOOP.raw_os_error(),
                ));
            }
            let target = readlinkat(&inspected, "", Vec::new()).map_err(errno_to_io)?;
            let target = PathBuf::from(OsString::from_vec(target.into_bytes()));
            let target_components = if target.is_absolute() {
                current = Arc::clone(root);
                absolute_target_components(root_path, &target)?
            } else {
                components(&target, &target)?
            };
            prepend(&mut pending, target_components);
            continue;
        }

        if is_final {
            return finalize(
                inspected,
                inspected_stat,
                access,
                lexical_path,
                Some(current),
            );
        }

        if file_type != FileType::Directory {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("not a directory: {}", lexical_path.display()),
            ));
        }
        current = Arc::new(DirNode {
            fd: inspected,
            parent: Some(current),
        });
    }
}

/// Finishes resolving one final component: validates its type, then — for
/// anything other than an identity probe or a not-followed symlink —
/// reopens the *exact same inode* via `/proc/self/fd/<n>` instead of a
/// second name-based open. Naming the already-open descriptor's own
/// magic-link closes the validate-then-open race a concurrent replacement
/// (e.g. a regular file swapped for a FIFO) could otherwise win: there is
/// no second pathname lookup left for it to win against. See the hashFiles
/// hardening review, finding 7, and `fifo(7)` on why a plain second
/// `open()` by name remains exploitable even with `O_NONBLOCK`.
///
/// `inspected` is `O_PATH`-only and, when the target is a directory,
/// becomes that directory's held [`DirNode`] (parented by `parent`) so
/// later children resolve relative to it; the reopened descriptor is a
/// separate, independent readable handle used only for this one
/// enumeration or hash.
fn finalize(
    inspected: OwnedFd,
    inspected_stat: Stat,
    access: Access,
    lexical_path: &Path,
    parent: Option<Arc<DirNode>>,
) -> io::Result<Resolved> {
    let file_type = FileType::from_raw_mode(inspected_stat.st_mode);
    if matches!(access, Access::Identity) || file_type == FileType::Symlink {
        // Identity probes and a not-followed final symlink never need a
        // readable handle; returning the `O_PATH` descriptor as-is keeps
        // the invariant that hashFiles never obtains a readable fd to
        // anything but a validated regular file or directory.
        return Ok(Resolved {
            target: OpenedTarget {
                fd: inspected,
                stat: inspected_stat,
            },
            directory: None,
        });
    }
    validate_type(lexical_path, access, &inspected_stat)?;
    let magic_link = format!("/proc/self/fd/{}", inspected.as_raw_fd());
    // The magic-link target must be followed (unlike every other open in
    // this module) — it names the fd we already hold, not an
    // attacker-influenced pathname, so following it cannot be raced by a
    // workspace rename: no name lookup under the workspace happens here.
    let reopened =
        rustix::fs::open(magic_link, access_flags(access), Mode::empty()).map_err(errno_to_io)?;
    let stat = fstat(&reopened).map_err(errno_to_io)?;
    if (stat.st_dev, stat.st_ino) != (inspected_stat.st_dev, inspected_stat.st_ino) {
        return Err(io::Error::other(
            "hashFiles reopened a different inode than the one it validated",
        ));
    }
    validate_type(lexical_path, access, &stat)?;
    let directory = (file_type == FileType::Directory).then(|| {
        Arc::new(DirNode {
            fd: inspected,
            parent,
        })
    });
    Ok(Resolved {
        target: OpenedTarget { fd: reopened, stat },
        directory,
    })
}

pub(super) fn validate_type(path: &Path, access: Access, stat: &Stat) -> io::Result<()> {
    let file_type = FileType::from_raw_mode(stat.st_mode);
    let valid = match access {
        Access::Identity => true,
        Access::ReadDirectory => file_type == FileType::Directory,
        Access::ReadFile => file_type == FileType::RegularFile,
    };
    if valid {
        return Ok(());
    }
    let (kind, expected) = match access {
        Access::Identity => (io::ErrorKind::InvalidInput, "filesystem object"),
        Access::ReadDirectory => (io::ErrorKind::NotADirectory, "directory"),
        Access::ReadFile => (io::ErrorKind::InvalidInput, "regular file"),
    };
    Err(io::Error::new(
        kind,
        format!("not a {expected}: {}", path.display()),
    ))
}

fn absolute_target_components(root_path: &Path, target: &Path) -> io::Result<VecDeque<OsString>> {
    // Accept the runner-relevant absolute spelling of a target beneath the
    // canonical workspace. Following it would otherwise require leaving the
    // held-root namespace merely to prove it eventually returns; `current`
    // is already reset to the root node by the caller, so rejecting
    // anything whose lexical spelling doesn't fall under the workspace here
    // keeps that reset meaningful. Absolute aliases elsewhere are rejected.
    let relative = target
        .strip_prefix(root_path)
        .map_err(|_| outside_workspace(target))?;
    components(relative, target)
}

pub(super) fn components(path: &Path, original: &Path) -> io::Result<VecDeque<OsString>> {
    let mut result = VecDeque::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => result.push_back(name.to_os_string()),
            Component::CurDir => result.push_back(OsString::from(".")),
            Component::ParentDir => result.push_back(OsString::from("..")),
            Component::RootDir | Component::Prefix(_) => return Err(outside_workspace(original)),
        }
    }
    Ok(result)
}

fn prepend(pending: &mut VecDeque<OsString>, prefix: VecDeque<OsString>) {
    for component in prefix.into_iter().rev() {
        pending.push_front(component);
    }
}
