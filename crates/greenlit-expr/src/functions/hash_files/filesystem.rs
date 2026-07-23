//! The injectable filesystem boundary and its production implementation.

mod real;

use std::io;
use std::path::{Path, PathBuf};

pub use self::real::RealFs;

/// The kind of a directory entry returned while enumerating an
/// [`OpenedDir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (to a file or directory — not yet resolved).
    Symlink,
    /// A non-regular filesystem node such as a FIFO, socket, or device.
    /// `hashFiles()` never reads or hashes these nodes.
    Other,
}

/// One directory entry: a bare file name (not a full path) plus its kind.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// The entry's bare name within its parent directory.
    pub name: String,
    /// What kind of entry this is.
    pub kind: EntryKind,
}

/// A directory opened as a capability rather than a reusable path.
///
/// Every child of an [`OpenedDir`] must be reached through
/// [`OpenedDir::open_child_dir`] or [`OpenedDir::hash_child_file`] — never
/// by re-deriving a lexical path and reopening it against the workspace
/// root. [`RealFs`] resolves a child through the exact kernel object this
/// directory names, so a concurrent rename of an ancestor cannot
/// substitute a different, unvetted directory or file for later access.
/// See `ARCHITECTURE.md`'s "Phase 1 trust and resource boundaries".
pub struct OpenedDir<'a> {
    /// Stable identity used by the bounded symbolic-link alias registry.
    pub identity: PathBuf,
    directory: Box<dyn OpenDirectory<'a> + 'a>,
}

impl<'a> OpenedDir<'a> {
    /// Wraps `directory` as one capability, identified by `identity`.
    #[must_use]
    pub fn new(identity: PathBuf, directory: Box<dyn OpenDirectory<'a> + 'a>) -> Self {
        Self {
            identity,
            directory,
        }
    }

    /// Pulls the next direct child (bare name + kind) in native
    /// enumeration order.
    pub fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        self.directory.next_entry()
    }

    /// Opens the child directory named `name` (one bare path component —
    /// implementations reject a name containing a path separator) relative
    /// to this held directory. `lexical_path` is `name`'s full lexical
    /// position and is used only for error messages, never for access.
    /// `follow` controls whether a symlink child is followed to its target;
    /// a symlink or plain entry that does not resolve to a directory
    /// returns `ErrorKind::NotADirectory` so the caller can retry it as a
    /// file. Passing `follow: false` for an entry whose listed kind was
    /// already a plain directory additionally rejects it if a concurrent
    /// rename replaced it with a symlink in the meantime, regardless of the
    /// caller's `--follow-symbolic-links` mode.
    pub fn open_child_dir(
        &self,
        name: &str,
        lexical_path: &Path,
        follow: bool,
    ) -> io::Result<OpenedDir<'a>> {
        self.directory.open_child_dir(name, lexical_path, follow)
    }

    /// Computes the SHA-256 digest of the regular file named `name`,
    /// relative to this held directory, following a final symlink when
    /// `follow` is true. `lexical_path` is used only for error messages.
    /// `check_timeout` is polled the same way as the top-level deadline
    /// checks.
    pub fn hash_child_file(
        &self,
        name: &str,
        lexical_path: &Path,
        follow: bool,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        self.directory
            .hash_child_file(name, lexical_path, follow, check_timeout)
    }
}

/// The object backing one [`OpenedDir`]. Implementations are shared with a
/// bounded filesystem worker; an instance is created and consumed entirely
/// on that one worker, so this trait carries no thread-safety bound.
pub trait OpenDirectory<'a> {
    /// Pulls the next direct child (bare name + kind) in native
    /// enumeration order.
    fn next_entry(&mut self) -> io::Result<Option<DirEntry>>;

    /// See [`OpenedDir::open_child_dir`].
    fn open_child_dir(
        &self,
        name: &str,
        lexical_path: &Path,
        follow: bool,
    ) -> io::Result<OpenedDir<'a>>;

    /// See [`OpenedDir::hash_child_file`].
    fn hash_child_file(
        &self,
        name: &str,
        lexical_path: &Path,
        follow: bool,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]>;
}

/// Filesystem access for `hashFiles()`, injected so evaluation never talks
/// to host filesystem APIs directly — the real implementation is [`RealFs`];
/// tests substitute an in-memory fake. Implementations are shared with a
/// bounded filesystem worker and must therefore be thread-safe at this
/// top level; an opened directory is created and consumed entirely on that
/// one worker (see [`OpenDirectory`]).
pub trait HashFilesFs: std::fmt::Debug + Send + Sync {
    /// The directory `hashFiles` patterns are rooted against by default
    /// (GitHub's `GITHUB_WORKSPACE` equivalent).
    fn workspace_root(&self) -> &Path;
    /// The directory a leading `~`/`~/…` pattern roots against. `None` if
    /// unavailable, in which case such a pattern contributes zero matches.
    fn home_dir(&self) -> Option<&Path> {
        None
    }
    /// Opens one search-root ENTRY POINT — the workspace root, `$HOME`, or a
    /// literal search root derived from a pattern's non-glob prefix — by its
    /// full lexical path. Called once per entry point; every subsequent
    /// child access must go through the returned [`OpenedDir`] instead of
    /// calling this again with a longer path, so a rename of an ancestor
    /// after it was already inspected cannot substitute an unvetted
    /// directory for later traversal.
    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>>;
    /// Returns the kind of one entry point without following a final
    /// symbolic link. Never used for a child of an already-open directory
    /// (its kind comes from that directory's own entry stream).
    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind>;
    /// Computes the SHA-256 digest of one entry point that is itself a
    /// regular file (a literal search root with no wildcard segment).
    /// Implementations must call `check_timeout` around blocking work and
    /// between bounded read chunks.
    fn hash_file_sha256(
        &self,
        path: &Path,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]>;
}
