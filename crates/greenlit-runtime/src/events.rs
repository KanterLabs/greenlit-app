//! Structured execution events and job-scoped log output.
//!
//! The executor reports state through these ports rather than asking callers
//! to infer job and step transitions from human text. Dynamic values have
//! already passed through the run masker before they reach this boundary.

use std::io;
use std::time::Duration;

use greenlit_engine::Conclusion;

/// A stable scope for one concrete job instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobScope {
    /// Authored workflow job id.
    pub job_id: String,
    /// Run-unique matrix/job instance key.
    pub instance_id: String,
    /// Resolved display name.
    pub display: String,
}

/// The authored kind of a top-level workflow step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepEventKind {
    /// A shell/script step.
    Run,
    /// An action invocation.
    Uses {
        /// Authored action reference.
        reference: String,
    },
    /// A synthesized action pre phase.
    ActionPre {
        /// Authored action reference.
        reference: String,
    },
    /// A synthesized action post phase.
    ActionPost {
        /// Action/post display reference.
        reference: String,
    },
}

/// One structured state transition emitted during execution.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ExecutionEvent {
    /// A job instance entered execution.
    JobStarted {
        /// Concrete job scope.
        scope: JobScope,
    },
    /// A job was skipped before creating resources.
    JobSkipped {
        /// Concrete job scope.
        scope: JobScope,
        /// Human explanation of the evaluated condition.
        reason: String,
    },
    /// A job reached its terminal result.
    JobFinished {
        /// Concrete job scope.
        scope: JobScope,
        /// Terminal conclusion.
        conclusion: Conclusion,
        /// Job wall time.
        duration: Duration,
    },
    /// A step body is about to run.
    StepStarted {
        /// Concrete job scope.
        scope: JobScope,
        /// Unique key within the job instance (`step-2`, `pre-2`, `post-0`).
        event_id: String,
        /// Stable ordinal within the report, including synthesized phases.
        index: usize,
        /// Authored id when present.
        step_id: Option<String>,
        /// Masked display label.
        label: String,
        /// Step/action kind.
        kind: StepEventKind,
    },
    /// A step was skipped.
    StepSkipped {
        /// Concrete job scope.
        scope: JobScope,
        /// Unique key within the job instance.
        event_id: String,
        /// Stable ordinal within the report.
        index: usize,
        /// Authored id when present.
        step_id: Option<String>,
        /// Masked display label.
        label: String,
        /// Human explanation of the condition decision.
        reason: String,
    },
    /// A step reached its terminal conclusion.
    StepFinished {
        /// Concrete job scope.
        scope: JobScope,
        /// Unique key within the job instance.
        event_id: String,
        /// Stable ordinal within the report.
        index: usize,
        /// Authored id when present.
        step_id: Option<String>,
        /// Masked display label.
        label: String,
        /// Outcome before `continue-on-error`.
        outcome: Conclusion,
        /// Conclusion after `continue-on-error`.
        conclusion: Conclusion,
        /// Step wall time.
        duration: Duration,
    },
}

/// Receives typed execution transitions.
pub trait ExecutionEventSink: Send {
    /// Records one event. Implementations must preserve call order.
    fn on_event(&mut self, event: ExecutionEvent);
}

/// A sink used by embedders that do not need lifecycle events.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExecutionEventNull;

impl ExecutionEventSink for ExecutionEventNull {
    fn on_event(&mut self, _event: ExecutionEvent) {}
}

/// Receives bytes attributed to one concrete job instance.
///
/// Bytes are already masked. They can still arrive at arbitrary chunk
/// boundaries, so consumers must assemble lines independently per
/// `instance_id`.
pub trait RunLogSink: Send {
    /// Writes one chunk and returns the number of bytes accepted.
    fn write(&mut self, scope: &JobScope, bytes: &[u8]) -> io::Result<usize>;

    /// Flushes buffered output.
    fn flush(&mut self) -> io::Result<()>;
}

/// Adapts a normal writer to [`RunLogSink`] for compatibility embedders.
pub struct FlatLogSink<'a> {
    out: &'a mut (dyn io::Write + Send),
}

impl<'a> FlatLogSink<'a> {
    /// Creates an adapter that discards job attribution.
    pub fn new(out: &'a mut (dyn io::Write + Send)) -> Self {
        Self { out }
    }
}

impl RunLogSink for FlatLogSink<'_> {
    fn write(&mut self, _scope: &JobScope, bytes: &[u8]) -> io::Result<usize> {
        self.out.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}
