//! Engine-side execution semantics: the pure logic that drives a `run:`
//! workflow once the container runtime hands raw step results back.
//!
//! This module is the runner "brain" beneath the container boundary. It owns
//! no Docker and performs no I/O; the `greenlit-runtime` crate executes the
//! resolved invocations this module produces and returns exit codes and
//! command-file contents, which this module folds back into the live
//! contexts. Each submodule transcribes a documented GitHub Actions rule and
//! cites it:
//!
//! - [`shell`] — shell resolution (`bash -e {0}` default, `sh -e {0}` in
//!   containers, explicit `shell:`/`defaults.run.shell`).
//! - [`env`](mod@env) — per-step env layering and the runner/`github` default
//!   variables.
//! - [`command_files`] — `GITHUB_ENV`/`GITHUB_OUTPUT`/`GITHUB_PATH`/
//!   `GITHUB_STEP_SUMMARY` parsing, including heredoc blocks.
//! - [`log_commands`] — `::group::`/`::error::`/`::add-mask::` parsing and the
//!   running secret [`log_commands::Masker`].
//! - [`outcome`] — `outcome`/`conclusion`, `continue-on-error`, the implicit
//!   `success()` gate, and the rolling job-status roll-up.
//! - [`contexts`] — building the live `steps`/`needs` contexts and merging
//!   matrix-leg outputs.
//! - [`job_outputs`] — finalizing declared job outputs with secret redaction
//!   and the size cap.
//! - [`resolve`] — finishing plan-time deferred values/conditions against the
//!   live runtime context.

pub mod command_files;
pub mod contexts;
pub mod env;
pub mod job_outputs;
pub mod log_commands;
pub mod outcome;
pub mod resolve;
pub mod shell;

use indexmap::IndexMap;

use crate::execution::shell::ShellInvocation;

/// A fully resolved `run:` step, ready for the container runtime to execute.
///
/// The runtime writes [`StepInvocation::script`] to a temporary file, whose
/// path the [`ShellInvocation`] already references via its substituted `{0}`
/// placeholder, then execs [`ShellInvocation::program`] with
/// [`ShellInvocation::args`] under [`StepInvocation::env`] and
/// [`StepInvocation::working_directory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepInvocation {
    /// The resolved shell program and arguments.
    pub shell: ShellInvocation,
    /// The script body written to the temporary script file.
    pub script: String,
    /// The fully layered environment for this step.
    pub env: IndexMap<String, String>,
    /// The resolved working directory, if the step or defaults set one.
    pub working_directory: Option<String>,
}

pub use contexts::{NeedRecord, StepRecord, build_needs_context, build_steps_context};
pub use log_commands::{
    LogLine, MASK_REGISTRATION_FAILURE_DIAGNOSTIC, MaskRegistrationError, Masker,
    SensitiveValueRegistry, SensitiveValueSnapshot,
};
pub use outcome::{Conclusion, StepExit, StepResult};
pub use shell::{ShellInvocation as ResolvedShell, ShellSelection};
