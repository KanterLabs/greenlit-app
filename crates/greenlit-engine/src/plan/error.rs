//! Errors returned while assembling an execution plan.

use greenlit_workflow::Span;

use crate::graph::{GraphError, JobId};
use crate::matrix::MatrixError;
use crate::partial_eval::{LocatedEvalError, PartialEvalError};
use crate::runner::RunnerError;

/// Everything that can go wrong planning a workflow.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// The authored-reference inventory could not be rebuilt from a
    /// caller-mutated workflow model. Parsed workflows have already passed
    /// the same expression validation before reaching the planner.
    #[error("workflow static extraction failed: {source}")]
    StaticExtraction {
        /// Underlying workflow-expression error.
        #[source]
        source: Box<greenlit_workflow::ParseError>,
    },
    /// A recognized-but-rejected construct (`concurrency`, `workflow_call`,
    /// `environment`, a reusable-workflow call job, or OIDC permission) was
    /// present.
    /// `greenlit-workflow` parses these successfully; planning is where
    /// v0's precise rejection happens (`PHASE-1-engine-core.md`).
    #[error("{span}: {name}: not in v0")]
    NotSupportedInV0 {
        /// The construct's name.
        name: &'static str,
        /// Where it appears.
        span: Span,
    },
    /// A workflow-level context-sensitive field could not be folded.
    #[error("{span}: workflow field: {source}")]
    WorkflowEval {
        /// Exact authored field that failed evaluation.
        span: Span,
        /// Underlying expression failure.
        #[source]
        source: Box<PartialEvalError>,
    },
    /// The `needs` graph could not be built.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// A job's `strategy:` could not be resolved. Boxed per
    /// `clippy::result_large_err`: [`MatrixError`] carries `Span`s and
    /// `Vec<DeferReason>`s, which would otherwise inflate every
    /// `Result<_, PlanError>` return value in this crate.
    #[error("job '{job}': {source}")]
    Matrix {
        /// The job.
        job: JobId,
        /// The underlying failure.
        #[source]
        source: Box<MatrixError>,
    },
    /// A job's (or leg's) `runs-on:` could not be resolved. Boxed for the
    /// same reason as [`PlanError::Matrix`].
    #[error("job '{job}': {source}")]
    Runner {
        /// The job.
        job: JobId,
        /// The underlying failure.
        #[source]
        source: Box<RunnerError>,
    },
    /// A job's or step's `if:`/`env:`/`outputs:`/`name:` could not be
    /// folded. Boxed for the same reason as [`PlanError::Matrix`].
    #[error("{span}: job '{job}'{}: {source}", step.as_ref().map(|s| format!(" step '{s}'")).unwrap_or_default())]
    Eval {
        /// Exact authored field that failed evaluation.
        span: Span,
        /// The job.
        job: JobId,
        /// The step id, if this happened while planning a step rather than
        /// the job itself.
        step: Option<String>,
        /// The underlying failure.
        #[source]
        source: Box<PartialEvalError>,
    },
    /// A job-level `if:` referenced a context or special function GitHub
    /// does not make available for that workflow key.
    #[error(
        "{span}: job '{job}' `if:` uses {unavailable}, which is not available in `jobs.<job_id>.if` — move this check to a step-level `if:` condition"
    )]
    JobConditionUnavailable {
        /// The job containing the invalid condition.
        job: JobId,
        /// The unavailable context/function, formatted for the diagnostic.
        unavailable: String,
        /// The authored `if:` field's source span.
        span: Span,
    },
    /// The retained local plan would exceed Greenlit's bounded in-process
    /// representation. GitHub dispatches matrix jobs separately; Greenlit
    /// must retain all legs locally, so it applies this local safety bound.
    #[error("{span}: expanded execution plan exceeds Greenlit's {max_bytes}-byte local size limit")]
    SizeLimit {
        /// The job or field whose expansion crossed the limit.
        span: Span,
        /// The maximum stable-JSON byte count retained by one plan.
        max_bytes: usize,
    },
}

/// Internal bridge for helpers that both evaluate one authored field and
/// charge it to the retained-plan budget.
pub(crate) enum RetainedFieldError {
    Evaluation(LocatedEvalError),
    Limit(PlanError),
}

impl From<LocatedEvalError> for RetainedFieldError {
    fn from(error: LocatedEvalError) -> Self {
        Self::Evaluation(error)
    }
}

impl From<PlanError> for RetainedFieldError {
    fn from(error: PlanError) -> Self {
        Self::Limit(error)
    }
}

pub(super) fn eval_err(
    job: &JobId,
    step: Option<&str>,
    span: Span,
    source: PartialEvalError,
) -> PlanError {
    PlanError::Eval {
        span,
        job: job.clone(),
        step: step.map(str::to_string),
        source: Box::new(source),
    }
}

pub(super) fn located_eval_err(
    job: &JobId,
    step: Option<&str>,
    error: LocatedEvalError,
) -> PlanError {
    eval_err(job, step, error.span, error.source)
}

pub(super) fn retained_field_err(
    job: &JobId,
    step: Option<&str>,
    error: RetainedFieldError,
) -> PlanError {
    match error {
        RetainedFieldError::Evaluation(error) => located_eval_err(job, step, error),
        RetainedFieldError::Limit(error) => error,
    }
}
