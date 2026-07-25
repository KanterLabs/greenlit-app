//! Flattening a resolved [`ExecutionPlan`] into the concrete job instances the
//! executor runs, in topological order.
//!
//! A non-matrix job is one instance; a statically expanded matrix job is one
//! instance per leg, each carrying its own resolved runner, container, env,
//! steps, and `matrix` context value. Runtime-materialized matrices retain a
//! template until their direct dependencies complete, then use those real
//! outputs to create concrete instances.

use indexmap::IndexMap;

use crate::executor::ExecError;
use greenlit_engine::execution::{NeedRecord, build_needs_context};
use greenlit_engine::{
    Condition, ContainerPlan, DEFAULT_MAX_MATRIX_LEGS, EnvValue, Evaluation, ExecutionPlan, JobId,
    JobOutputsPlan, JobPlan, MatrixLeg, MatrixPlan, MatrixValue, Planned, RunDefaultsPlan,
    RunnerImage, StaticSkip, StepPlan, materialize_controls, materialize_matrix,
    materialize_runner,
};
use greenlit_expr::{Context, RunStatus, Value};

/// One concrete job instance ready to run. Borrows the plan; no plan data is
/// cloned beyond the small computed `matrix` value.
#[derive(Clone)]
pub(crate) struct JobInstance<'a> {
    /// The instance's resolved display name.
    pub display: &'a Planned<String>,
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
    /// Deterministic dependency wave.
    pub wave: u32,
    /// Complete template retained for runtime materialization.
    pub job: &'a JobPlan,
}

/// A job group after dependency outputs have materialized its matrix and
/// scheduling controls.
pub(crate) struct MaterializedGroup<'a> {
    /// Concrete instances in GitHub creation order.
    pub instances: Vec<JobInstance<'a>>,
    /// GitHub's matrix fail-fast policy.
    pub fail_fast: bool,
    /// Maximum legs from this group that may execute concurrently.
    pub max_parallel: usize,
}

/// Expand `plan` into job groups in topological (start) order.
///
/// # Errors
///
/// Returns [`ExecError::DeferredRunner`] only when a non-matrix job somehow
/// retains a runtime-only runner label without a dependency materialization
/// point.
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
            wave: job.wave,
            job,
        });
    }
    Ok(groups)
}

/// Expand one job plan into its instances.
fn expand_job(job: &JobPlan) -> Result<Vec<JobInstance<'_>>, ExecError> {
    let matrix_legs = match &job.strategy.matrix {
        Some(MatrixPlan::Deferred { .. }) => return Ok(Vec::new()),
        Some(MatrixPlan::Static { legs, .. }) => Some(legs.as_slice()),
        None => None,
    };

    if job.legs.is_empty() {
        // Non-matrix job (or a zero-leg static matrix, which produces nothing).
        if matrix_legs.is_some() {
            return Ok(Vec::new());
        }
        let Some(runner) = static_runner(job.runner.as_ref()) else {
            return Ok(Vec::new());
        };
        return Ok(vec![JobInstance {
            display: &job.name,
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
        let Some(runner) = static_runner(Some(&leg.runner)) else {
            return Ok(Vec::new());
        };
        let matrix = legs.get(index).map_or(Value::Null, matrix_leg_value);
        instances.push(JobInstance {
            display: &leg.name,
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

/// Materialize a group after all direct dependencies have completed.
pub(crate) fn materialize<'a>(
    group: &'a JobGroup<'a>,
    roots: &super::context::ContextRoots,
    needs: &[NeedRecord],
) -> Result<MaterializedGroup<'a>, ExecError> {
    let base_context = Context::new(std::sync::Arc::clone(&roots.fs))
        .with_github(roots.github.clone())
        .with_vars(roots.vars.clone())
        .with_inputs(roots.inputs.clone())
        .with_secrets(roots.secrets.clone())
        .with_needs(build_needs_context(needs))
        .with_status(RunStatus::Success);
    let (fail_fast, max_parallel) = materialize_controls(&group.job.strategy, &base_context)
        .map_err(|source| ExecError::MatrixRuntime { source })?;

    if !group.job.strategy.is_matrix_deferred() {
        if group.instances.is_empty() && group.job.strategy.matrix.is_none() {
            let runner_plan =
                group
                    .job
                    .runner
                    .as_ref()
                    .ok_or_else(|| ExecError::Infrastructure {
                        message: format!("job '{}' has no retained runner template", group.id.0),
                        fix: "preserve the run evidence and file a Greenlit defect".to_string(),
                    })?;
            let runner = materialize_runner(runner_plan, &base_context)
                .map_err(|source| ExecError::RunnerRuntime { source })?;
            return Ok(MaterializedGroup {
                instances: vec![JobInstance {
                    display: &group.job.name,
                    runner,
                    container: group.job.container.as_ref(),
                    services: &group.job.services,
                    condition: group.job.condition.as_ref(),
                    implicit_status_gate: group.job.implicit_status_gate,
                    skip: group.job.skip.as_ref(),
                    job_env: &group.job.env,
                    defaults: &group.job.defaults,
                    outputs: &group.job.outputs,
                    steps: &group.job.steps,
                    matrix: Value::Null,
                }],
                fail_fast,
                max_parallel: 1,
            });
        }
        if group.instances.is_empty()
            && let Some(MatrixPlan::Static { legs, .. }) = &group.job.strategy.matrix
            && !legs.is_empty()
        {
            let mut instances = Vec::with_capacity(legs.len());
            for (leg, plan) in legs.iter().zip(&group.job.legs) {
                let matrix = matrix_leg_value(leg);
                let runner_context = base_context
                    .clone()
                    .with_matrix(matrix.clone())
                    .with_strategy(strategy_context(
                        fail_fast,
                        max_parallel,
                        leg.index,
                        legs.len(),
                    ));
                let runner = materialize_runner(&plan.runner, &runner_context)
                    .map_err(|source| ExecError::RunnerRuntime { source })?;
                instances.push(JobInstance {
                    display: &plan.name,
                    runner,
                    container: plan.container.as_ref(),
                    services: &plan.services,
                    condition: plan.condition.as_ref(),
                    implicit_status_gate: group.job.implicit_status_gate,
                    skip: plan.skip.as_ref(),
                    job_env: &plan.env,
                    defaults: &plan.defaults,
                    outputs: &plan.outputs,
                    steps: &plan.steps,
                    matrix,
                });
            }
            return Ok(MaterializedGroup {
                instances,
                fail_fast,
                max_parallel: max_parallel
                    .map_or_else(|| legs.len().max(1), |value| value.get() as usize),
            });
        }
        return Ok(MaterializedGroup {
            instances: group.instances.clone(),
            fail_fast,
            max_parallel: max_parallel.map_or_else(
                || group.instances.len().max(1),
                |value| value.get() as usize,
            ),
        });
    }

    let legs = materialize_matrix(&group.job.strategy, &base_context, DEFAULT_MAX_MATRIX_LEGS)
        .map_err(|source| ExecError::MatrixRuntime { source })?;
    let mut instances = Vec::with_capacity(legs.len());
    for leg in &legs {
        let matrix = matrix_leg_value(leg);
        let runner_context = base_context
            .clone()
            .with_matrix(matrix.clone())
            .with_strategy(strategy_context(
                fail_fast,
                max_parallel,
                leg.index,
                legs.len(),
            ));
        let runner_plan = group
            .job
            .runner
            .as_ref()
            .ok_or_else(|| ExecError::Infrastructure {
                message: format!(
                    "runtime matrix job '{}' has no retained runner template",
                    group.id.0
                ),
                fix: "preserve the run evidence and file a Greenlit defect".to_string(),
            })?;
        let runner = materialize_runner(runner_plan, &runner_context)
            .map_err(|source| ExecError::RunnerRuntime { source })?;
        instances.push(JobInstance {
            display: &group.job.name,
            runner,
            container: group.job.container.as_ref(),
            services: &group.job.services,
            condition: group.job.condition.as_ref(),
            implicit_status_gate: group.job.implicit_status_gate,
            skip: group.job.skip.as_ref(),
            job_env: &group.job.env,
            defaults: &group.job.defaults,
            outputs: &group.job.outputs,
            steps: &group.job.steps,
            matrix,
        });
    }
    Ok(MaterializedGroup {
        instances,
        fail_fast,
        max_parallel: max_parallel.map_or_else(|| legs.len().max(1), |value| value.get() as usize),
    })
}

fn strategy_context(
    fail_fast: bool,
    max_parallel: Option<std::num::NonZeroU32>,
    index: usize,
    total: usize,
) -> Value {
    Value::object(vec![
        ("fail-fast".to_string(), Value::Bool(fail_fast)),
        ("job-index".to_string(), Value::Number(index as f64)),
        ("job-total".to_string(), Value::Number(total as f64)),
        (
            "max-parallel".to_string(),
            Value::Number(max_parallel.map_or(total.max(1) as f64, |value| f64::from(value.get()))),
        ),
    ])
}

/// Extract a statically-known runner image.
fn static_runner(runner: Option<&Planned<RunnerImage>>) -> Option<RunnerImage> {
    match runner.map(|planned| &planned.evaluation) {
        Some(Evaluation::Static(image)) => Some(*image),
        _ => None,
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
