//! The root-relative resolver every access — entry point or child — walks
//! through, re-derived fresh from the one pinned root on every single call.
//!
//! Finding 1 of the hashFiles hardening re-verification: an earlier revision
//! held each ancestor directory as its own `openat2`-opened descriptor,
//! cached for as long as its `OpenedDir` stayed on the traversal stack, and
//! resolved a child relative to that held fd. `RESOLVE_BENEATH` is scoped to
//! the dirfd actually passed to `openat2`, not to some original root, so once
//! a held ancestor's name was renamed out from under the workspace (e.g.
//! `workspace/a` renamed to `outside/a`), the held fd still named the exact
//! same (now-relocated) inode and further opens relative to it kept
//! succeeding — reading outside the workspace despite every individual
//! `openat2` call being, in isolation, "correct".
//!
//! The fix: never cache an intermediate ancestor descriptor across separate
//! accesses. Every open instead re-walks the *entire* chain from the one
//! pinned workspace root, one path component and one `openat2` call per hop,
//! discarding each intermediate descriptor the moment the next hop has used
//! it. `RESOLVE_BENEATH` (plus `RESOLVE_NO_MAGICLINKS`/`RESOLVE_NO_XDEV`, see
//! `filesystem/real.rs`) makes each individual hop fail outright — `ENOENT`,
//! not a silent escape — the instant its own immediate parent is no longer
//! what it was expected to be, and re-deriving every ancestor hop-by-hop on
//! every access means a rename anywhere along the chain is caught, not just
//! at the final component. This is the real invariant: "every object was
//! reached via a path beneath the root at its open instant", re-checked by
//! the kernel on every single access rather than assumed to still hold from
//! an earlier check.
//!
//! One hop per hashFiles path component (not one `openat2` call for a
//! *joined* multi-component path string) is deliberate: the deep-traversal
//! invariant already pins a lexical chain whose *joined* length exceeds
//! Linux's `PATH_MAX`, which a single combined-path `openat2` call would
//! reject outright (`ENAMETOOLONG`) long before any containment check ran.
//! Walking hop-by-hop keeps every individual syscall argument bounded by one
//! component's own name length regardless of how deep the chain is, at the
//! cost of `O(depth)` syscalls per access instead of `O(1)` — an acceptable
//! trade given hashFiles' own security invariant requires re-validating the
//! whole ancestor chain on every access anyway.
//!
//! Directory *enumeration* keeps reading from its own already-open `Dir`
//! handle (see `directory.rs`) — only further *access* (a child open, or a
//! file hash) goes through this resolver again. `DirNode`'s held-ancestor
//! chain and its `..`-via-parent-pointer are gone with it: the accumulated
//! relative path (a plain `PathBuf`, walked hop-by-hop from root on demand)
//! is enough to support `..` (pop a component) without ever asking the
//! kernel to resolve it, and a directory's continuation state for its own
//! future children is just that same relative path, not a descriptor.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

use rustix::fd::{AsRawFd, OwnedFd};
use rustix::fs::{FileType, Mode, OFlags, Stat, fstat, readlinkat};
use rustix::io::fcntl_dupfd_cloexec;

use super::{Access, access_flags, errno_to_io, open_component, outside_workspace};

const MAX_SYMLINKS: usize = 40;

pub(super) struct OpenedTarget {
    pub(super) fd: OwnedFd,
    pub(super) stat: Stat,
}

/// The outcome of [`resolve`]: a readable target descriptor, plus — only
/// when that target is itself a directory — its own path relative to the
/// workspace root, ready to seed further root-relative resolution for its
/// children.
pub(super) struct Resolved {
    pub(super) target: OpenedTarget,
    pub(super) directory: Option<PathBuf>,
}

/// Resolves `pending` (one or more path components, e.g. a single child
/// name, or more after a symbolic link target expands it) starting from
/// `start` — the workspace-root-relative path of an already-inspected
/// directory (empty for the workspace root itself). Before touching
/// `pending` at all, `start` itself is re-walked hop-by-hop, fresh from
/// `root`, via [`open_chain`] — never relative to a held intermediate
/// descriptor from some earlier call — so a rename of any ancestor since it
/// was last inspected surfaces as `ENOENT` here rather than silently
/// resolving through the stale object (finding 1 of the hashFiles hardening
/// re-verification).
pub(super) fn resolve(
    root: &OwnedFd,
    root_path: &Path,
    start: &Path,
    mut pending: VecDeque<OsString>,
    follow_final: bool,
    access: Access,
    lexical_path: &Path,
) -> io::Result<Resolved> {
    let mut accumulated = start.to_path_buf();
    let mut symlinks = 0_usize;

    loop {
        // Every iteration re-derives the current directory fresh from
        // `root`, one hop per already-accumulated component: see the module
        // doc for why this, rather than a held descriptor or a single
        // joined-path `openat2` call, is both the actual security fix and
        // the only way to stay within `PATH_MAX` for an arbitrarily deep
        // chain.
        let current = open_chain(root, &accumulated)?;

        let Some(component) = pending.pop_front() else {
            // Resolving `start` itself (e.g. an entry point whose lexical
            // path was exactly an already-open directory).
            let inspected_stat = fstat(&current).map_err(errno_to_io)?;
            return finalize(current, inspected_stat, access, lexical_path, accumulated);
        };
        if component == OsStr::new(".") {
            continue;
        }
        if component == OsStr::new("..") {
            if !accumulated.pop() {
                return Err(outside_workspace(lexical_path));
            }
            continue;
        }

        let inspected = open_component(
            &current,
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
                accumulated = PathBuf::new();
                absolute_target_components(root_path, &target)?
            } else {
                components(&target, &target)?
            };
            prepend(&mut pending, target_components);
            continue;
        }

        let candidate = accumulated.join(&component);
        if is_final {
            return finalize(inspected, inspected_stat, access, lexical_path, candidate);
        }

        if file_type != FileType::Directory {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("not a directory: {}", lexical_path.display()),
            ));
        }
        accumulated = candidate;
    }
}

/// Walks `relative` (workspace-root-relative; empty meaning the root
/// itself) from `root`, one path component and one `openat2` call per hop,
/// discarding each intermediate descriptor as soon as the next hop has used
/// it. Every call re-derives the whole chain — nothing from a previous call
/// is ever reused — which is what actually closes finding 1: a rename of
/// any ancestor, at any depth, since the last time it was inspected, breaks
/// the very next hop through it (`ENOENT`) instead of silently resolving
/// through a stale intermediate. Splitting the walk into single-component
/// hops (rather than one `openat2` call over the whole joined path) also
/// keeps every individual syscall argument bounded by `NAME_MAX`, never
/// `PATH_MAX`, regardless of how deep `relative` is.
fn open_chain(root: &OwnedFd, relative: &Path) -> io::Result<OwnedFd> {
    let mut current = fcntl_dupfd_cloexec(root, 0).map_err(errno_to_io)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            // `relative` is always built from `components()`'s own Normal-
            // only output (see below) plus workspace-root-relative
            // directory paths this module itself produced; any other
            // component would mean a caller-supplied path leaked in
            // unnormalized, which must never silently resolve.
            return Err(outside_workspace(relative));
        };
        current = open_component(
            &current,
            name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        )?;
    }
    Ok(current)
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
/// `inspected` is `O_PATH`-only; when the target is a directory, `at`
/// (its own workspace-root-relative path) becomes the seed later children
/// resolve from — never the descriptor itself, so nothing about this
/// directory is trusted for access once this call returns; the reopened
/// descriptor is a separate, independent readable handle used only for this
/// one enumeration or hash.
fn finalize(
    inspected: OwnedFd,
    inspected_stat: Stat,
    access: Access,
    lexical_path: &Path,
    at: PathBuf,
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
    let directory = (file_type == FileType::Directory).then_some(at);
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
    // canonical workspace. `RESOLVE_BENEATH` itself rejects an absolute
    // symlink target outright (see `filesystem/real.rs`'s known-issues
    // citation), so this manual rewrite — stripping the workspace prefix
    // and continuing root-relative resolution with what remains — is the
    // only way to keep that documented allowance without ever asking the
    // kernel to follow the absolute spelling directly. Absolute aliases
    // elsewhere are rejected.
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
