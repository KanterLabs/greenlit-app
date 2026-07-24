//! Run-phase progress port: the events a run emits while it prepares to
//! execute steps.
//!
//! Between the job header and the first streamed step line sit the run's
//! long silent phases — image pull or build, container boot, and workspace
//! isolation (which can be a full checkout copy). Engines and the executor
//! report those phases through [`ProgressSink`]; the CLI renders them on
//! stderr so stdout stays the machine-parseable run log. Events mirror the
//! timed-stage vocabulary (`image-ensure`, `container-boot`,
//! `overlay-setup`) so what the user watches matches the end-of-run timing
//! breakdown.
//!
//! Emitters do not throttle: they report every observation (a pull emits one
//! event per daemon chunk). Cadence is the rendering boundary's decision.

/// One observation from a run's preparation phases.
///
/// Every string field is untrusted display text (daemon output, or values
/// that can embed repository-authored content) — renderers must escape it
/// before it reaches a terminal.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    /// An image pull began.
    PullStarted {
        /// The image reference being pulled.
        image: String,
    },
    /// Cumulative pull progress, summed across layers.
    PullProgress {
        /// Bytes downloaded so far across all layers.
        current_bytes: u64,
        /// Total bytes across all layers, once every layer reported one.
        total_bytes: Option<u64>,
    },
    /// The pull completed.
    PullFinished {
        /// The image reference that was pulled.
        image: String,
    },
    /// An image build began.
    BuildStarted {
        /// The tag being built.
        tag: String,
    },
    /// One trimmed, non-empty line of daemon build output.
    BuildLine {
        /// The build-log line, verbatim after trimming.
        line: String,
    },
    /// The build completed.
    BuildFinished {
        /// The tag that was built.
        tag: String,
    },
    /// Container creation and start began.
    BootStarted,
    /// The container is running (workspace isolation not yet established).
    BootFinished,
    /// Workspace isolation progress inside the container.
    Workspace(WorkspaceProgress),
    /// A job's pinned Node action runtime bundle(s) are being ensured
    /// (checked against the local cache, downloaded and checksum-verified
    /// on a miss) — the `action-runtime-ensure` stage
    /// (`crate::executor::actions::node_runtime`).
    ActionRuntimeEnsureStarted,
    /// The action runtime ensure finished; every Node action in the job can
    /// now run.
    ActionRuntimeEnsureFinished,
}

/// Progress of the in-container workspace isolation (`overlay-setup` stage).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceProgress {
    /// Unprivileged overlayfs was unavailable; the checkout is being copied
    /// in instead. Emitted once, when the fallback is first observed —
    /// renderers should keep this line permanently visible.
    FellBack {
        /// The errno name the mount attempt failed with (e.g. `EPERM`).
        reason: String,
    },
    /// The copy-in fallback is progressing.
    Copying {
        /// Files copied so far.
        files: u64,
        /// Bytes copied so far.
        bytes: u64,
    },
    /// The workspace is established and steps can execute.
    Ready {
        /// The isolation strategy that ended up in effect
        /// (`overlay` / `copy-in`).
        strategy: String,
    },
}

/// Receives [`ProgressEvent`]s as a run's preparation phases advance.
///
/// The rendering counterpart to [`crate::engine::ExecOutputSink`]: the CLI
/// implements this to draw phase status; embedders that do not render pass
/// [`ProgressNull`].
pub trait ProgressSink: Send {
    /// One preparation-phase observation arrived.
    fn on_progress(&mut self, event: ProgressEvent);
}

/// A sink that discards all progress — for callers that do not render it.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProgressNull;

impl ProgressSink for ProgressNull {
    fn on_progress(&mut self, _event: ProgressEvent) {}
}
