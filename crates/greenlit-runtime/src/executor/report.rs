//! The structured outcome of a run: per-step and per-job results and timings.
//!
//! The executor streams live logs while it works but returns this data-only
//! report at the end; the presentation layer (`litci run` in `greenlit-app`)
//! renders the end-of-run table and per-stage breakdown from it plus the
//! metrics record, keeping this crate free of terminal presentation.

use std::time::Duration;

use indexmap::IndexMap;

use greenlit_engine::Conclusion;

/// One executed (or skipped) step's result.
#[derive(Debug, Clone)]
pub struct StepReport {
    /// Display label (`name:`, else the `run:` first line, else `step <n>`).
    pub label: String,
    /// The step's `outcome` (before `continue-on-error`).
    pub outcome: Conclusion,
    /// The step's `conclusion` (after `continue-on-error`).
    pub conclusion: Conclusion,
    /// Wall-clock duration of the step's exec (zero for a skipped step).
    pub duration: Duration,
    /// Whether the step's body actually executed.
    pub ran: bool,
}

/// One job instance's result.
#[derive(Debug, Clone)]
pub struct JobReport {
    /// Stable job id.
    pub id: String,
    /// Resolved display name (matrix legs include their suffix).
    pub display: String,
    /// The job's terminal result.
    pub result: Conclusion,
    /// Each step's report, in declaration order.
    pub steps: Vec<StepReport>,
    /// The finalized job outputs exposed to dependents.
    pub outputs: IndexMap<String, String>,
    /// Wall-clock duration from container boot through the last step.
    pub duration: Duration,
    /// This job's container id, kept alive rather than torn down, when the
    /// run was started with `--write-back` (`RunConfig::write_back`) — `None`
    /// for a skipped job (no container ever existed) or when write-back was
    /// not requested (the container is already removed by the time the
    /// report is built). The caller uses this to export/confirm/apply each
    /// ran job's overlay diff after the whole run finishes.
    pub container_id: Option<String>,
}

/// The whole run's structured result.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// Every job instance, in execution (topological) order.
    pub jobs: Vec<JobReport>,
    /// The run's overall conclusion: `failure` if any job failed, else
    /// `cancelled` if any was cancelled, else `success` (skipped-only counts as
    /// success, matching a workflow whose every job was skipped).
    pub overall: Conclusion,
}

impl RunReport {
    /// Whether the run should exit non-zero (any job failed or was cancelled).
    #[must_use]
    pub fn failed(&self) -> bool {
        matches!(self.overall, Conclusion::Failure | Conclusion::Cancelled)
    }

    /// Derive the overall conclusion from the per-job results.
    pub(crate) fn overall_of(jobs: &[JobReport]) -> Conclusion {
        if jobs
            .iter()
            .any(|job| matches!(job.result, Conclusion::Failure))
        {
            Conclusion::Failure
        } else if jobs
            .iter()
            .any(|job| matches!(job.result, Conclusion::Cancelled))
        {
            Conclusion::Cancelled
        } else {
            Conclusion::Success
        }
    }
}
