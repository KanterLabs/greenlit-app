//! The crate's error type.
//!
//! Every variant states what happened; the `fix:` half of the
//! what-happened-plus-what-to-do pair `AGENTS.md` requires is attached by
//! `greenlit-app`'s diagnostic renderer, which owns user-facing phrasing for
//! every crate.

use std::path::PathBuf;

/// A failure in one of the local stores.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// `HOME` was not set, so the per-user store root cannot be located.
    #[error("the HOME environment variable is not set")]
    HomeDirUnavailable,

    /// `HOME` was set to something other than an absolute path.
    #[error("the HOME environment variable is not an absolute path")]
    InvalidHomeDir,

    /// A filesystem operation on the store failed.
    #[error("{operation} failed for {}: {source}", path.display())]
    Io {
        /// The operation being attempted, in the imperative ("create the
        /// cache entry directory").
        operation: &'static str,
        /// The path the operation targeted.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// A stored metadata document could not be read back.
    ///
    /// The store repairs this by ignoring the entry rather than failing the
    /// run: a torn `meta.json` means one unusable cache entry, not a broken
    /// installation. This variant exists for callers that ask for one entry
    /// by name and genuinely cannot proceed without it.
    #[error("the stored metadata at {} is not readable: {source}", path.display())]
    CorruptMetadata {
        /// The metadata document that could not be parsed.
        path: PathBuf,
        /// The underlying deserialization failure.
        #[source]
        source: serde_json::Error,
    },

    /// A caller-supplied name would escape the store root.
    ///
    /// Cache keys, artifact names, and versions all originate in workflow
    /// YAML, which `AGENTS.md` treats as untrusted: they are hashed rather
    /// than used as path components, and this variant guards the remaining
    /// caller-constructed components as defense in depth.
    #[error("{kind} {value:?} is not a usable path component")]
    InvalidComponent {
        /// What sort of name was rejected ("artifact name").
        kind: &'static str,
        /// The rejected value.
        value: String,
    },

    /// An upload referenced a reservation that does not exist, or that was
    /// already committed.
    ///
    /// `@actions/cache` only ever `PATCH`es or commits an id the same run
    /// just reserved, so reaching this means the client and store disagree
    /// about run state.
    #[error("cache reservation {id} is not open")]
    UnknownReservation {
        /// The reservation id the client sent.
        id: i64,
    },

    /// A cache entry already exists for this exact key and version.
    ///
    /// The hosted service answers a duplicate reservation with HTTP 409 and
    /// `actions/cache` treats it as "someone else saved this first", which is
    /// a successful no-op rather than a failure.
    #[error("a cache entry already exists for key {key:?} at this version")]
    AlreadyReserved {
        /// The key whose reservation collided.
        key: String,
    },
}
