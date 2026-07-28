//! Error type for every fallible operation this crate exposes.

use std::path::PathBuf;

/// Everything that can go wrong opening, writing, or reading the local
/// metrics store.
///
/// Every variant carries the path (and, where relevant, the line number)
/// involved so a caller can render a precise, actionable message per
/// `AGENTS.md`'s UX invariant ("every error maps to a state plus the one
/// action that fixes it") without this crate needing to know how
/// `greenlit-app` wants to format it.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    /// [`MetricsStore::open_default`](crate::MetricsStore::open_default) could
    /// not resolve a home directory because the `HOME` environment variable
    /// is unset. The fix is either to set `HOME` or to call
    /// [`MetricsStore::at`](crate::MetricsStore::at) with an explicit path.
    #[error(
        "could not determine the user home directory (the HOME environment \
         variable is not set) — set HOME, or pass an explicit metrics file \
         path instead of using the default store"
    )]
    HomeDirUnavailable,

    /// `HOME` was present but did not name an absolute user-home path. An
    /// empty or relative value would redirect the supposedly user-local
    /// metrics store into the invocation's repository.
    #[error(
        "could not determine the user home directory (HOME is empty, relative, inaccessible, or not a directory) — set HOME to an accessible absolute user home directory"
    )]
    InvalidHomeDir,

    /// Creating the metrics directory (`~/.litci/metrics` by default) failed.
    #[error("failed to create metrics directory {path}: {source}")]
    CreateDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Opening the NDJSON file for appending failed.
    #[error("failed to open metrics file {path} for writing: {source}")]
    OpenForWrite {
        /// The file that could not be opened.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The metrics path resolved to a symbolic link, directory, FIFO,
    /// device, or another non-regular file. Following or blocking on such a
    /// target would violate the local-file boundary.
    #[error(
        "metrics path component {path} is a symbolic link or special file — replace it with a real directory or regular file owned by the current user"
    )]
    InvalidPathComponent {
        /// The unsafe or unsupported path component.
        path: PathBuf,
    },

    /// A writable metrics directory or file grants access to group or other
    /// users. Greenlit refuses to append rather than silently trusting or
    /// repairing a path it did not create.
    #[error(
        "metrics path component {path} has unsafe mode {mode:#05o} — restrict the directory to 0700 or the file to 0600, then retry"
    )]
    UnsafePermissions {
        /// The path whose permission bits are too broad.
        path: PathBuf,
        /// The observed Unix permission bits.
        mode: u32,
    },

    /// A persistent metrics path is owned by another user. Greenlit never
    /// repairs or writes through such a path.
    #[error(
        "metrics path component {path} is owned by uid {owner}, not current uid {expected} — move it aside or restore the expected owner, then retry"
    )]
    UnsafeOwner {
        /// The path with the unexpected owner.
        path: PathBuf,
        /// Owner recorded on the inode.
        owner: u32,
        /// Effective uid of the current process.
        expected: u32,
    },

    /// Acquiring the cross-process lock that serializes append/repair and
    /// excludes readers during mutation failed.
    #[error("failed to lock metrics file {path}: {source}")]
    LockFile {
        /// The metrics file that could not be locked.
        path: PathBuf,
        /// The underlying locking error.
        source: std::io::Error,
    },

    /// Serializing a record to JSON failed. In practice this cannot happen
    /// for the record types this crate defines (no non-finite floats, no
    /// non-string map keys), but serialization is still a fallible API that
    /// must be propagated rather than unwrapped.
    #[error("failed to serialize metrics record for {path}: {source}")]
    Serialize {
        /// The metrics file the record was going to be appended to.
        path: PathBuf,
        /// The underlying serialization error.
        source: serde_json::Error,
    },

    /// A newly produced record exceeded the bounded on-disk record size.
    #[error("metrics record for {path} exceeds the {max_bytes}-byte per-record safety limit")]
    RecordWriteLimit {
        /// The metrics file the record was going to be appended to.
        path: PathBuf,
        /// Maximum accepted serialized record bytes, excluding its newline.
        max_bytes: usize,
    },

    /// Writing the serialized record line to disk failed.
    #[error("failed to write metrics record to {path}: {source}")]
    WriteRecord {
        /// The file the write was targeting.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Reading the NDJSON file failed for a reason other than "it does not
    /// exist yet" (which is treated as an empty history, not an error).
    #[error("failed to read metrics file {path}: {source}")]
    ReadFile {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// One stored record or unterminated tail exceeded the bounded size
    /// Greenlit can safely parse or repair.
    #[error("metrics record at {path} exceeds the {max_bytes}-byte per-record safety limit")]
    RecordReadLimit {
        /// The metrics file containing the oversized record or tail.
        path: PathBuf,
        /// Maximum accepted record bytes, excluding its newline.
        max_bytes: usize,
    },

    /// A newline-terminated line in the NDJSON file did not parse as a valid
    /// [`InvocationRecord`](crate::InvocationRecord). Since this file is
    /// written exclusively by [`MetricsStore::append`](crate::MetricsStore::append),
    /// this indicates corruption rather than an unterminated torn final
    /// append, which the metrics-store readers tolerate within the per-record
    /// bound.
    #[error("corrupt metrics record at {path}:{line} — {source}")]
    CorruptRecord {
        /// The metrics file containing the bad line.
        path: PathBuf,
        /// The 1-based line number of the bad record.
        line: usize,
        /// The underlying parse error.
        source: serde_json::Error,
    },

    /// A newest-window record found by bounded reverse reading was malformed.
    /// Reverse reading deliberately does not count every older newline, so a
    /// byte offset is truthful where a line number would not be.
    #[error("corrupt metrics record at {path} byte offset {offset} — {source}")]
    CorruptRecentRecord {
        /// The metrics file containing the bad record.
        path: PathBuf,
        /// Byte offset where the record begins.
        offset: u64,
        /// The underlying parse error.
        source: serde_json::Error,
    },

    /// A well-formed record uses a schema version this build cannot read.
    #[error(
        "unsupported metrics schema version {found} at {path}:{line} (this build supports {supported})"
    )]
    UnsupportedSchema {
        /// Metrics file containing the record.
        path: PathBuf,
        /// 1-based record line.
        line: usize,
        /// Version found on disk.
        found: u64,
        /// Version this build understands.
        supported: u32,
    },

    /// A newest-window record found by bounded reverse reading uses a schema
    /// version this build cannot read.
    #[error(
        "unsupported metrics schema version {found} at {path} byte offset {offset} (this build supports {supported})"
    )]
    UnsupportedRecentSchema {
        /// Metrics file containing the record.
        path: PathBuf,
        /// Byte offset where the record begins.
        offset: u64,
        /// Version found on disk.
        found: u64,
        /// Version this build understands.
        supported: u32,
    },
}
