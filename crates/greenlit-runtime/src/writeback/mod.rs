//! `--write-back`: export the overlay diff, let the user review and confirm it,
//! then apply it to the host working tree.
//!
//! `PHASE-2-execution.md` ("Overlay isolation"): after the run, export the
//! upper-layer diff, list the changed paths, and require confirmation before the
//! Greenlit host process applies it; the workflow container never receives host
//! write access, and `--no-input` rejects `--write-back` since no non-interactive
//! approval mechanism is specified for v0. Cancellation leaves the host
//! untouched.
//!
//! The container-side isolation lives in [`crate::isolation`]; this module is
//! the host-side half — it runs entirely in the Greenlit process, which is the
//! only thing that ever writes the host tree.

mod diff;
mod error;
mod host_fs;

use std::io::{BufRead, Write};
use std::path::Path;

use crate::engine::ContainerEngine;
use crate::isolation::OVERLAY_UPPER_EXPORT_PATH;

pub use diff::{Change, ChangeKind, OverlayDiff};
pub use error::{NoInputConflict, WriteBackError};

/// Reject `--write-back` when `--no-input` is set.
///
/// # Errors
///
/// Returns [`NoInputConflict`] (carrying its fix message) when both are
/// requested; write-back's confirmation prompt cannot run without interaction.
pub fn validate_request(write_back: bool, no_input: bool) -> Result<(), NoInputConflict> {
    if write_back && no_input {
        Err(NoInputConflict)
    } else {
        Ok(())
    }
}

/// The confirmation gate: shown the change list, decide whether to apply.
///
/// The production implementation prompts the user
/// ([`InteractiveConfirm`]); tests inject a decision. This is the boundary that
/// guarantees the host tree is never written without explicit approval.
pub trait Confirm {
    /// Return `true` to apply the listed changes, `false` to cancel.
    fn confirm(&mut self, changes: &[Change]) -> bool;
}

/// What a write-back attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteBackOutcome {
    /// The overlay held no changes; nothing to apply.
    NoChanges,
    /// The user declined at the confirmation prompt; the host is untouched.
    Cancelled,
    /// The listed changes were applied to the host working tree.
    Applied(Vec<Change>),
}

/// Export the overlay diff from `container`, confirm it, and apply it to
/// `host_root`.
///
/// The host tree is written only after `confirm` returns `true`; on `false`
/// (cancellation) or an empty diff nothing is written.
///
/// # Errors
///
/// Returns a [`WriteBackError`] if the export, archive read, or apply fails, or
/// if the diff names a path that escapes `host_root`.
pub async fn run_write_back(
    engine: &dyn ContainerEngine,
    container: &str,
    host_root: &Path,
    confirm: &mut dyn Confirm,
) -> Result<WriteBackOutcome, WriteBackError> {
    let tar = engine
        .export_path(container, OVERLAY_UPPER_EXPORT_PATH)
        .await
        .map_err(WriteBackError::Export)?;
    let diff = OverlayDiff::new(tar);
    let changes = diff.changes()?;
    if changes.is_empty() {
        return Ok(WriteBackOutcome::NoChanges);
    }
    if !confirm.confirm(&changes) {
        // Cancellation must leave the host exactly as it was.
        return Ok(WriteBackOutcome::Cancelled);
    }
    diff.apply(host_root)?;
    Ok(WriteBackOutcome::Applied(changes))
}

/// The production [`Confirm`]: list the changes and read a yes/no answer.
///
/// The change list is written to `out` (stderr in production, kept off stdout so
/// it never contaminates machine-readable output) and the answer read from
/// `input` (stdin). Any read failure is treated as "no", so an unreadable prompt
/// never applies changes silently.
pub struct InteractiveConfirm<R: BufRead, W: Write> {
    input: R,
    out: W,
    host_root: std::path::PathBuf,
}

impl<R: BufRead, W: Write> InteractiveConfirm<R, W> {
    /// Build a confirmer over the given input/output and the working-tree root
    /// the changes will apply to (shown in the prompt).
    pub fn new(input: R, out: W, host_root: impl Into<std::path::PathBuf>) -> Self {
        InteractiveConfirm {
            input,
            out,
            host_root: host_root.into(),
        }
    }
}

impl<R: BufRead, W: Write> Confirm for InteractiveConfirm<R, W> {
    fn confirm(&mut self, changes: &[Change]) -> bool {
        let _ = writeln!(
            self.out,
            "--write-back will apply {} change(s) to {}:",
            changes.len(),
            self.host_root.display()
        );
        for change in changes {
            let _ = writeln!(
                self.out,
                "  {} {}",
                change.kind.sigil(),
                change.path.display()
            );
        }
        let _ = write!(self.out, "Apply these changes to your working tree? [y/N] ");
        let _ = self.out.flush();

        let mut answer = String::new();
        if self.input.read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}
