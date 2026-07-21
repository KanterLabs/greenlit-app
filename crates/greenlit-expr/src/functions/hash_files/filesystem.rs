//! The injectable filesystem boundary and its production implementation.

use std::path::{Path, PathBuf};

/// The kind of a directory entry returned by [`HashFilesFs::read_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (to a file or directory — not yet resolved).
    Symlink,
}

/// One directory entry: a bare file name (not a full path) plus its kind.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// The entry's bare name within its parent directory.
    pub name: String,
    /// What kind of entry this is.
    pub kind: EntryKind,
}

/// Filesystem access for `hashFiles()`, injected so evaluation never talks
/// to `std::fs` directly — the real implementation is [`RealFs`]; tests
/// substitute an in-memory fake.
pub trait HashFilesFs: std::fmt::Debug {
    /// The directory `hashFiles` patterns are rooted against by default
    /// (GitHub's `GITHUB_WORKSPACE`).
    fn workspace_root(&self) -> &Path;
    /// The directory a leading `~`/`~/…` pattern roots against. `None` if
    /// unavailable, in which case such a pattern contributes zero matches
    /// rather than erroring (real runners always have a home directory; this
    /// is a defensive default for environments that don't).
    fn home_dir(&self) -> Option<&Path> {
        None
    }
    /// Lists `path`'s direct children (bare names + kind), not recursive.
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>>;
    /// Reads a regular file's full contents.
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// Returns the kind of `path` without following a final symbolic link.
    ///
    /// The default preserves compatibility for injected implementations
    /// that predate this method. Implementations with metadata access should
    /// override it so symlinks can be distinguished precisely.
    fn entry_kind(&self, path: &Path) -> std::io::Result<EntryKind> {
        match self.read_dir(path) {
            Ok(_) => Ok(EntryKind::Dir),
            Err(dir_error) => match self.read_file(path) {
                Ok(_) => Ok(EntryKind::File),
                Err(_) => Err(dir_error),
            },
        }
    }
    /// Resolves symbolic links for directory-cycle detection.
    ///
    /// The identity default is suitable for virtual filesystems with no
    /// symlinks. Real filesystems should override it with canonicalization.
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}

/// The production [`HashFilesFs`], backed directly by `std::fs`, rooted at a
/// given workspace directory.
#[derive(Debug)]
pub struct RealFs {
    root: PathBuf,
    home: Option<PathBuf>,
}

impl RealFs {
    /// Builds a real filesystem rooted at `root` (GitHub's `GITHUB_WORKSPACE`
    /// equivalent). Reads `$HOME` once at construction for `~`-rooted
    /// patterns.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        RealFs {
            root: root.into(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

impl HashFilesFs for RealFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn home_dir(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let kind = if file_type.is_symlink() {
                EntryKind::Symlink
            } else if file_type.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }
        Ok(entries)
    }

    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn entry_kind(&self, path: &Path) -> std::io::Result<EntryKind> {
        let file_type = std::fs::symlink_metadata(path)?.file_type();
        if file_type.is_symlink() {
            Ok(EntryKind::Symlink)
        } else if file_type.is_dir() {
            Ok(EntryKind::Dir)
        } else {
            Ok(EntryKind::File)
        }
    }

    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
}
