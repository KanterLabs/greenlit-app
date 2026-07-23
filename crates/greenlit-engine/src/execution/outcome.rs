//! Step outcome model: `outcome` vs `conclusion`, `continue-on-error`, the
//! implicit `success()` gate, and the job's rolling status roll-up.
//!
//! GitHub's runner keeps a rolling job status (`JobContext.Status`, initially
//! `success`) that the status-check functions evaluate against, advances it to
//! `failure` when a step fails without `continue-on-error`, and to `cancelled`
//! on cancellation. Each step exposes two results: `outcome` (the raw result
//! before `continue-on-error`) and `conclusion` (after it). See
//! [`StepsRunner.cs`](https://github.com/actions/runner/blob/main/src/Runner.Worker/StepsRunner.cs)
//! and GitHub's
//! [`steps` context](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#steps-context).

use serde::Serialize;

use greenlit_expr::RunStatus;

use crate::execution::contexts::NeedRecord;

/// The four terminal states a step or job result can take. GitHub spells them
/// `success`, `failure`, `cancelled`, and `skipped`; the same set populates a
/// step's `outcome`/`conclusion` and a job's `needs.<id>.result`.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#steps-context>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Conclusion {
    /// The step/job completed successfully.
    Success,
    /// The step/job failed.
    Failure,
    /// The run was cancelled before or during the step/job.
    Cancelled,
    /// The step/job was not executed because its condition (or an upstream
    /// skip) suppressed it.
    Skipped,
}

impl Conclusion {
    /// GitHub's exact lowercase spelling, matching what an expression sees in
    /// `steps.<id>.conclusion` / `needs.<id>.result`.
    pub fn as_github_str(self) -> &'static str {
        match self {
            Conclusion::Success => "success",
            Conclusion::Failure => "failure",
            Conclusion::Cancelled => "cancelled",
            Conclusion::Skipped => "skipped",
        }
    }
}

/// How a step's body terminated once it actually ran. Distinct from
/// [`Conclusion`] because `continue-on-error` and the never-ran cases are
/// applied on top of this raw signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepExit {
    /// The process exited `0`.
    Success,
    /// The process exited non-zero.
    Failed,
    /// The step exceeded its `timeout-minutes` and was killed. GitHub treats a
    /// timed-out step as a failure.
    /// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepstimeout-minutes>
    TimedOut,
    /// The step was killed by run cancellation while executing.
    Cancelled,
}

/// A step's resolved `outcome` and `conclusion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepResult {
    /// The result before `continue-on-error` is applied.
    pub outcome: Conclusion,
    /// The result after `continue-on-error` is applied; this is what feeds the
    /// rolling job status.
    pub conclusion: Conclusion,
}

/// Decides whether a step's body executes, given the rolling job status, the
/// authored `if:` result, and whether the step's `if:` carried an explicit
/// status-check function (in which case the implicit `success()` gate is *not*
/// applied — the authored condition already accounts for status).
///
/// GitHub wraps a conditionless or status-function-free `if:` with an implicit
/// `success() &&`, so a step is skipped once any earlier step has failed
/// unless it opts back in with `always()`/`failure()`/`cancelled()`.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepsif>
pub fn step_activates(status: RunStatus, implicit_status_gate: bool, condition_true: bool) -> bool {
    if implicit_status_gate {
        // The implicit `success()`: the step runs only while nothing has
        // failed or been cancelled. Cancellation flows in through `status`.
        matches!(status, RunStatus::Success) && condition_true
    } else {
        condition_true
    }
}

/// Computes a step's `outcome`/`conclusion` for a step that ran, applying
/// `continue-on-error`: a failing step whose `continue-on-error` is `true` has
/// `outcome: failure` but `conclusion: success`. Cancellation is never
/// rescued by `continue-on-error`.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#steps-context>
pub fn step_result_from_exit(exit: StepExit, continue_on_error: bool) -> StepResult {
    match exit {
        StepExit::Success => StepResult {
            outcome: Conclusion::Success,
            conclusion: Conclusion::Success,
        },
        StepExit::Failed | StepExit::TimedOut => StepResult {
            outcome: Conclusion::Failure,
            conclusion: if continue_on_error {
                Conclusion::Success
            } else {
                Conclusion::Failure
            },
        },
        StepExit::Cancelled => StepResult {
            outcome: Conclusion::Cancelled,
            conclusion: Conclusion::Cancelled,
        },
    }
}

/// The result for a step whose activation gate did not fire: both `outcome`
/// and `conclusion` are `skipped`.
pub const fn step_result_skipped() -> StepResult {
    StepResult {
        outcome: Conclusion::Skipped,
        conclusion: Conclusion::Skipped,
    }
}

/// Advances the rolling job status after a step concludes.
///
/// The status is monotone toward severity: a `failure` conclusion moves a
/// `success` job to `failure`; a `cancelled` conclusion moves it to
/// `cancelled` and is sticky. A later successful or skipped step never
/// resets an already-failed or already-cancelled job.
/// <https://github.com/actions/runner/blob/main/src/Runner.Worker/StepsRunner.cs>
pub fn advance_status(current: RunStatus, conclusion: Conclusion) -> RunStatus {
    match conclusion {
        Conclusion::Cancelled => RunStatus::Cancelled,
        Conclusion::Failure => match current {
            RunStatus::Cancelled => RunStatus::Cancelled,
            _ => RunStatus::Failure,
        },
        Conclusion::Success | Conclusion::Skipped => current,
    }
}

/// Maps the rolling job status to the job's terminal [`Conclusion`] for a job
/// that ran at least conditionally. A job whose own `if:` suppressed it (or
/// whose needed jobs were all skipped) is reported as
/// [`Conclusion::Skipped`] by the caller instead of going through here.
pub fn job_result_from_status(status: RunStatus) -> Conclusion {
    match status {
        RunStatus::Success => Conclusion::Success,
        RunStatus::Failure => Conclusion::Failure,
        RunStatus::Cancelled => Conclusion::Cancelled,
        // A job's own step-rolling status only ever moves between the three
        // arms above (see `advance_status`); `Blocked` is produced solely by
        // `needs_status` for a job-level `if:` and never reaches this call.
        // Map it conservatively to `Skipped` rather than assuming success.
        RunStatus::Blocked => Conclusion::Skipped,
    }
}

/// The status a job's `if:` condition (when the implicit `success()` gate is
/// not applied) evaluates against, derived from its direct dependencies.
///
/// GitHub computes this from the `needs` graph, not from steps: `failure()`
/// is true "if any ancestor job fails" (transitively, through the whole
/// dependency chain, even across an intermediate job that was itself skipped
/// because *its* ancestor failed); `success()` requires every direct
/// dependency to have actually concluded `success` — a skipped direct
/// dependency (that itself was not caused by an ancestor failure/
/// cancellation) makes both `success()` and `failure()` false, matching the
/// observed GitHub behavior that a job downstream of a skipped-not-failed
/// dependency does not run under an explicit `if: success()` either.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#status-check-functions>
pub fn needs_status(needs: &[NeedRecord]) -> RunStatus {
    if needs.iter().any(|need| need.chain_cancelled) {
        RunStatus::Cancelled
    } else if needs.iter().any(|need| need.chain_failed) {
        RunStatus::Failure
    } else if needs
        .iter()
        .all(|need| matches!(need.result, Conclusion::Success))
    {
        RunStatus::Success
    } else {
        RunStatus::Blocked
    }
}
