//! Flattening a resolved [`ExecutionPlan`] into the concrete job instances the
//! executor runs, in topological order.
//!
//! A non-matrix job is one instance; a statically expanded matrix job is one
//! instance per leg, each carrying its own resolved runner, container, env,
//! steps, and `matrix` context value. Runtime-materialized matrices and
//! runtime-derived runner labels depend on data that only exists mid-run;
//! `litci run` rejects them with a precise message rather than inventing a
//! value (they remain a later-phase capability).

use indexmap::IndexMap;

use greenlit_engine::{
    Condition, ContainerPlan, EnvValue, Evaluation, ExecutionPlan, JobId, JobOutputsPlan, JobPlan,
    MatrixLeg, MatrixPlan, MatrixValue, Planned, RunDefaultsPlan, RunnerImage, StaticSkip,
    StepPlan,
};
use greenlit_expr::Value;
use greenlit_workflow::Span;

use crate::executor::ExecError;

/// One concrete job instance ready to run. Borrows the plan; no plan data is
/// cloned beyond the small computed `matrix` value.
pub(crate) struct JobInstance<'a> {
    /// The instance's resolved display name.
    pub display: &'a Planned<String>,
    /// Direct dependencies (shared across all legs of a matrix job).
    pub needs: &'a [JobId],
    /// The resolved runner image.
    pub runner: RunnerImage,
    /// `container:`, if this job runs in a job container.
    pub container: Option<&'a ContainerPlan>,
    /// `services:`, keyed by service id, in file order.
    pub services: &'a IndexMap<String, ContainerPlan>,
    /// The job/leg `if:`, if authored.
    pub condition: Option<&'a Condition>,
    /// Whether the implicit `success()` gate applies to the job condition.
    pub implicit_status_gate: bool,
    /// A statically decided skip, if any.
    pub skip: Option<&'a StaticSkip>,
    /// This job's own `env:` layer (job or leg).
    pub job_env: &'a IndexMap<String, EnvValue>,
    /// Effective run defaults for this instance.
    pub defaults: &'a RunDefaultsPlan,
    /// The job-output finalization plan.
    pub outputs: &'a JobOutputsPlan,
    /// The step sequence, in file order.
    pub steps: &'a [StepPlan],
    /// The `matrix` context value (`Null` for a non-matrix job).
    pub matrix: Value,
}

/// All instances of one job id, in leg order (one for a non-matrix job).
pub(crate) struct JobGroup<'a> {
    /// The job id.
    pub id: JobId,
    /// This job's direct dependencies. Kept at the group level (not just per
    /// instance) so a zero-leg matrix job — which produces no
    /// [`JobInstance`] — still exposes its `needs` for ancestor-chain status
    /// propagation to its dependents.
    pub needs: &'a [JobId],
    /// Every instance of this job (one per matrix leg, or a single instance).
    pub instances: Vec<JobInstance<'a>>,
}

/// Expand `plan` into job groups in topological (start) order.
///
/// # Errors
///
/// Returns [`ExecError::DeferredMatrix`] for a runtime-materialized matrix or
/// [`ExecError::DeferredRunner`] for a runtime-derived `runs-on:` — both need
/// data that does not exist until mid-run and are out of scope for v0's
/// sequential executor.
pub(crate) fn expand(plan: &ExecutionPlan) -> Result<Vec<JobGroup<'_>>, ExecError> {
    let mut groups = Vec::with_capacity(plan.topo_order.len());
    for job_id in &plan.topo_order {
        let Some(job) = plan.jobs.iter().find(|job| &job.id == job_id) else {
            continue;
        };
        groups.push(JobGroup {
            id: job_id.clone(),
            needs: &job.needs,
            instances: expand_job(job)?,
        });
    }
    Ok(groups)
}

/// Expand one job plan into its instances.
fn expand_job(job: &JobPlan) -> Result<Vec<JobInstance<'_>>, ExecError> {
    let matrix_legs = match &job.strategy.matrix {
        Some(MatrixPlan::Deferred { span, .. }) => {
            return Err(ExecError::DeferredMatrix { span: span.clone() });
        }
        Some(MatrixPlan::Static { legs, .. }) => Some(legs.as_slice()),
        None => None,
    };

    if job.legs.is_empty() {
        // Non-matrix job (or a zero-leg static matrix, which produces nothing).
        if matrix_legs.is_some() {
            return Ok(Vec::new());
        }
        let runner = static_runner(job.runner.as_ref(), &job.span)?;
        return Ok(vec![JobInstance {
            display: &job.name,
            needs: &job.needs,
            runner,
            container: job.container.as_ref(),
            services: &job.services,
            condition: job.condition.as_ref(),
            implicit_status_gate: job.implicit_status_gate,
            skip: job.skip.as_ref(),
            job_env: &job.env,
            defaults: &job.defaults,
            outputs: &job.outputs,
            steps: &job.steps,
            matrix: Value::Null,
        }]);
    }

    // Statically expanded matrix: one instance per leg, aligned by index with
    // `strategy.matrix.legs`.
    let legs = matrix_legs.unwrap_or(&[]);
    let mut instances = Vec::with_capacity(job.legs.len());
    for (index, leg) in job.legs.iter().enumerate() {
        let runner = static_runner(Some(&leg.runner), &job.span)?;
        let matrix = legs.get(index).map_or(Value::Null, matrix_leg_value);
        instances.push(JobInstance {
            display: &leg.name,
            needs: &job.needs,
            runner,
            container: leg.container.as_ref(),
            services: &leg.services,
            condition: leg.condition.as_ref(),
            implicit_status_gate: job.implicit_status_gate,
            skip: leg.skip.as_ref(),
            job_env: &leg.env,
            defaults: &leg.defaults,
            outputs: &leg.outputs,
            steps: &leg.steps,
            matrix,
        });
    }
    Ok(instances)
}

/// Extract the statically-known runner image, or reject a runtime-deferred one.
fn static_runner(
    runner: Option<&Planned<RunnerImage>>,
    span: &Span,
) -> Result<RunnerImage, ExecError> {
    match runner.map(|planned| &planned.evaluation) {
        Some(Evaluation::Static(image)) => Ok(*image),
        _ => Err(ExecError::DeferredRunner { span: span.clone() }),
    }
}

/// Build a leg's `matrix` context object from its axis values.
fn matrix_leg_value(leg: &MatrixLeg) -> Value {
    Value::object(
        leg.values
            .iter()
            .map(|(key, value)| (key.as_str().to_string(), matrix_value_to_value(value)))
            .collect(),
    )
}

/// Convert a plan-time [`MatrixValue`] into an expression [`Value`].
fn matrix_value_to_value(value: &MatrixValue) -> Value {
    match value {
        MatrixValue::Null => Value::Null,
        MatrixValue::Bool(boolean) => Value::Bool(*boolean),
        MatrixValue::Number(number) => Value::Number(*number),
        MatrixValue::String(string) => Value::String(string.to_string()),
        MatrixValue::Sequence(items) => {
            Value::array(items.iter().map(matrix_value_to_value).collect())
        }
        MatrixValue::Mapping(entries) => Value::object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), matrix_value_to_value(value)))
                .collect(),
        ),
    }
}
