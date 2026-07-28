//! Symlink-safe host-filesystem writes for [`super::diff::OverlayDiff::apply`].
//!
//! `apply` used to join `host_root` with an archive-relative path and call
//! `std::fs::create_dir_all`/`File::create`/`fs::remove_file` on the joined
//! string — exactly like the OS's ordinary path lookup, both calls silently
//! *follow* any symlink an intermediate or leaf path component happens to
//! be. A repository can check in a symlink (`victim -> /outside/target`); if
//! a workflow's diff then writes a *regular file* at that same workspace
//! path, applying it naively would open (and truncate) whatever `victim`
//! points at, outside the repository (finding: write-back symlink
//! traversal).
//!
//! This module never resolves a destination through the kernel's ordinary,
//! symlink-following path lookup relative to an untrusted base. Every access
//! opens exactly one path component at a time, `*at()`-relative to a
//! directory descriptor this module has already verified is a real
//! directory (not a symlink to one) — the same descriptor-relative
//! discipline `greenlit_expr::functions::hash_files`'s `RealFs` uses to keep
//! traversal beneath a pinned root (see that module's doc comment for the
//! `openat2`/`RESOLVE_BENEATH` rationale this mirrors with plain `*at()`
//! hops, since write-back is a one-shot apply rather than a long traversal
//! that must also survive a concurrent rename). A pre-existing node at the
//! exact destination path is removed with `unlinkat` — which acts on the
//! directory entry itself and never follows a symlink — before the
//! replacement is created, so `--write-back` stays idempotent without ever
//! opening through a planted symlink.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path};

use rustix::fd::OwnedFd;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

/// A directory descriptor pinned at the write-back host root.
pub(crate) struct HostRoot {
    root: OwnedFd,
}

impl HostRoot {
    /// Opens `host_root` itself — an ordinary, trusted local path (the
    /// user's own repository checkout; not attacker-influenced) — as the
    /// pinned root descriptor every subsequent access resolves beneath.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if `host_root` cannot be opened as a
    /// directory.
    pub(crate) fn open(host_root: &Path) -> io::Result<Self> {
        let root = fs::open(
            host_root,
            OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(HostRoot { root })
    }

    /// Writes `contents` to the workspace-relative file `rel`, creating any
    /// missing parent directories and replacing (never following) an
    /// existing node at that exact path.
    pub(crate) fn write_file(&self, rel: &Path, contents: &mut dyn io::Read) -> io::Result<()> {
        let (parent, leaf) = self.open_parent_and_leaf(rel)?;
        remove_leaf_if_present(&parent, &leaf)?;
        let file_fd = fs::openat(
            &parent,
            &leaf,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            file_mode(),
        )?;
        let mut file = std::fs::File::from(file_fd);
        io::copy(contents, &mut file)?;
        Ok(())
    }

    /// Recreates a symlink at the workspace-relative path `rel` pointing at
    /// `target`, replacing any existing node there.
    pub(crate) fn write_symlink(&self, rel: &Path, target: &Path) -> io::Result<()> {
        let (parent, leaf) = self.open_parent_and_leaf(rel)?;
        remove_leaf_if_present(&parent, &leaf)?;
        fs::symlinkat(target, &parent, &leaf)?;
        Ok(())
    }

    /// Ensures the full workspace-relative directory chain `rel` exists.
    pub(crate) fn ensure_dir(&self, rel: &Path) -> io::Result<()> {
        self.open_dir_chain(normal_components(rel))?;
        Ok(())
    }

    /// Removes whatever exists at the workspace-relative path `rel` — a
    /// plain file or symlink is unlinked directly (never followed); a real
    /// directory is removed recursively, walking by descriptor the same way
    /// as every other access here. A missing parent or leaf is not an error
    /// (the deletion is already reflected).
    pub(crate) fn remove_path(&self, rel: &Path) -> io::Result<()> {
        let (parent, leaf) = match self.open_parent_and_leaf(rel) {
            Ok(pair) => pair,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        remove_leaf_if_present(&parent, &leaf)
    }

    /// Opens (creating if missing) the directory chain for `rel`'s *parent*,
    /// returning the parent descriptor and `rel`'s leaf name.
    fn open_parent_and_leaf(&self, rel: &Path) -> io::Result<(OwnedFd, OsString)> {
        let mut parts = normal_components(rel);
        let leaf = parts
            .pop()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty write-back path"))?;
        let parent = self.open_dir_chain(parts)?;
        Ok((parent, leaf))
    }

    /// Walks `parts` one component at a time from the pinned root,
    /// `openat`-relative to the previously opened (and verified-as-a-real-
    /// directory) descriptor, creating a missing component fresh. Never
    /// resolves more than one component per syscall, and never through an
    /// existing non-directory — in particular, never through a symlink:
    /// `O_NOFOLLOW` makes `openat` fail (`ELOOP`) rather than traverse one.
    fn open_dir_chain(&self, parts: Vec<OsString>) -> io::Result<OwnedFd> {
        let mut current = rustix::io::dup(&self.root)?;
        for part in parts {
            current = open_or_create_dir(&current, &part)?;
        }
        Ok(current)
    }
}

/// Permissions for a directory this module creates (`0o755`).
fn dir_mode() -> Mode {
    Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH
}

/// Permissions for a file this module creates (`0o644`).
fn file_mode() -> Mode {
    Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH
}

/// Opens `name` beneath `parent` as a directory, creating it (`mkdirat`) if
/// absent.
fn open_or_create_dir(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let open_flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match fs::openat(parent, name, open_flags, Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(Errno::NOENT) => {
            fs::mkdirat(parent, name, dir_mode())?;
            Ok(fs::openat(parent, name, open_flags, Mode::empty())?)
        }
        Err(errno) => Err(errno.into()),
    }
}

/// Removes whatever exists at `parent`/`leaf`. A plain file or symlink is
/// unlinked directly (never followed); a real directory is removed
/// recursively by descriptor. A missing leaf is not an error.
fn remove_leaf_if_present(parent: &OwnedFd, leaf: &OsStr) -> io::Result<()> {
    let stat = match fs::statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(errno) => return Err(errno.into()),
    };
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        remove_dir_recursive(parent, leaf)?;
        fs::unlinkat(parent, leaf, AtFlags::REMOVEDIR)?;
    } else {
        // Covers regular files *and* symlinks: without `REMOVEDIR`,
        // `unlinkat` removes the directory entry itself, never a symlink's
        // target.
        fs::unlinkat(parent, leaf, AtFlags::empty())?;
    }
    Ok(())
}

/// Recursively empties the real directory `parent`/`name` (its own removal
/// is the caller's job). Each child is resolved as one more `*at()` hop from
/// `name`'s own descriptor, so a symlinked child is unlinked, never
/// descended into.
fn remove_dir_recursive(parent: &OwnedFd, name: &OsStr) -> io::Result<()> {
    let dir_fd = fs::openat(
        parent,
        name,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let dir = fs::Dir::read_from(&dir_fd)?;
    let mut children = Vec::new();
    for entry in dir {
        let entry = entry?;
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        children.push(os_string_from_cstr(name));
    }
    for child in &children {
        remove_leaf_if_present(&dir_fd, child)?;
    }
    Ok(())
}

/// Converts a directory-entry `CStr` name to an [`OsString`] (Linux paths are
/// arbitrary non-NUL bytes; this never assumes UTF-8).
fn os_string_from_cstr(name: &std::ffi::CStr) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(name.to_bytes()).to_os_string()
}

/// The [`Component::Normal`] parts of `rel`, in order. `super::diff`'s
/// `workspace_relative` has already rejected every other component kind
/// (`..`, absolute, prefix) before a path ever reaches this module.
fn normal_components(rel: &Path) -> Vec<OsString> {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_os_string()),
            _ => None,
        })
        .collect()
}
