//! Errors for the `--write-back` flow.

use std::path::PathBuf;

use crate::error::RuntimeError;

/// `--write-back` was requested together with `--no-input`.
///
/// Write-back applies host-filesystem changes and therefore requires an
/// interactive confirmation (`PHASE-2-execution.md` "Overlay isolation": require
/// confirmation before the host process applies the diff). `--no-input`
/// disables that prompt, so the combination is rejected up front with the one
/// action that resolves it, per the `AGENTS.md` UX invariant. No dedicated
/// non-interactive approval mechanism is specified for v0.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`--write-back` needs interactive confirmation, which `--no-input` disables — \
     drop `--no-input` to review and confirm the changes, or omit `--write-back` \
     to leave the host working tree untouched"
)]
pub struct NoInputConflict;

/// Anything that can go wrong exporting, reading, or applying the overlay diff.
#[derive(Debug, thiserror::Error)]
pub enum WriteBackError {
    /// Lifting the overlay upper layer out of the container failed.
    #[error("could not export the overlay changes from the container: {0}")]
    Export(#[source] RuntimeError),

    /// The exported archive could not be read.
    #[error("could not read the exported overlay archive: {0}")]
    Read(#[source] std::io::Error),

    /// An archive entry named a path that escapes the workspace root (an
    /// absolute path or one containing `..`). Refused rather than applied, so a
    /// hostile overlay entry can never write outside the working tree.
    #[error("refusing to apply overlay change to unsafe path {path} — it escapes the workspace")]
    UnsafePath {
        /// The rejected entry path.
        path: PathBuf,
    },

    /// Applying one change to the host working tree failed.
    #[error("failed to apply overlay change at {path}: {source}")]
    Apply {
        /// The host path being written or removed.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}
