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

    /// A non-final line in the NDJSON file did not parse as a valid
    /// [`InvocationRecord`](crate::InvocationRecord). Since this file is
    /// written exclusively by [`MetricsStore::append`](crate::MetricsStore::append),
    /// this indicates corruption rather than a torn final append, which
    /// [`MetricsStore::read_all`](crate::MetricsStore::read_all) tolerates.
    #[error("corrupt metrics record at {path}:{line} — {source}")]
    CorruptRecord {
        /// The metrics file containing the bad line.
        path: PathBuf,
        /// The 1-based line number of the bad record.
        line: usize,
        /// The underlying parse error.
        source: serde_json::Error,
    },
}
