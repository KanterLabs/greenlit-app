//! The runtime error type for the container entrypoint.

use std::path::PathBuf;

use crate::strategy::{self, DecisionError, PathError};

/// Everything that can go wrong while establishing isolation and launching the
/// job command.
///
/// Each variant states what failed and, per `AGENTS.md`'s UX invariant, points
/// at the one thing that fixes it — even though these surface inside a
/// container, they end up in the user's run log.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// A directory the helper must create (the workspace mount point, or the
    /// overlay `upper`/`work` dirs) could not be created.
    #[error(
        "failed to create directory {path}: {source} — check the container's tmp mount is writable"
    )]
    PrepareDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// An overlay path component is not valid UTF-8, so it cannot be placed in
    /// the overlay option string.
    #[error("overlay path {path} is not valid UTF-8 — use an ASCII/UTF-8 workspace path")]
    NonUtf8Path {
        /// The offending path.
        path: PathBuf,
    },

    /// An overlay path component contains `,` or `:`, which overlayfs reserves
    /// as option and lower-layer separators.
    #[error(
        "overlay path {path} contains a ',' or ':' which overlayfs reserves — use a workspace path without those characters"
    )]
    UnsupportedPath {
        /// The offending path.
        path: PathBuf,
    },

    /// `--strategy overlay` was requested but the overlay mount failed. Overlay
    /// was mandatory, so the helper does not silently fall back.
    #[error(
        "overlay mount was required but failed ({errno_name}) — rerun with an overlay-capable engine or use --strategy auto to allow the copy-in fallback"
    )]
    OverlayRequired {
        /// The raw `errno` from `mount(2)`.
        errno: i32,
        /// A short name for that `errno`.
        errno_name: String,
    },

    /// An `--strategy auto` overlay attempt failed for a reason that is not
    /// "overlay unavailable", so falling back would hide a real problem.
    #[error(
        "overlay mount failed unexpectedly ({errno_name}) — this is not a missing-overlay-support error; check the --lower/--upper paths exist"
    )]
    MountFailed {
        /// The raw `errno` from `mount(2)`.
        errno: i32,
        /// A short name for that `errno`.
        errno_name: String,
    },

    /// Copying the checkout into the workspace (the fallback, or `--strategy
    /// copy-in`) failed.
    #[error("failed to copy the workspace at {path}: {source}")]
    CopyIn {
        /// The source or destination path being processed when the copy failed.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// `exec(2)` of the job command failed (e.g. the program was not found).
    /// Reaching this means isolation was already established.
    #[error(
        "failed to execute `{program}`: {source} — check the command exists in the container image"
    )]
    Exec {
        /// The program that could not be executed.
        program: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

impl From<DecisionError> for InitError {
    fn from(err: DecisionError) -> Self {
        match err {
            DecisionError::OverlayRequired { errno } => InitError::OverlayRequired {
                errno,
                errno_name: strategy::errno_name(errno),
            },
            DecisionError::MountFatal { errno } => InitError::MountFailed {
                errno,
                errno_name: strategy::errno_name(errno),
            },
        }
    }
}

impl From<PathError> for InitError {
    fn from(err: PathError) -> Self {
        match err {
            PathError::NonUtf8 { path } => InitError::NonUtf8Path { path },
            PathError::Separator { path } => InitError::UnsupportedPath { path },
        }
    }
}
