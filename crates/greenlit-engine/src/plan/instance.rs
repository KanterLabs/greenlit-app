//! Resolves all fields whose values can differ between concrete matrix
//! instances, or remain deferred on a dynamic-matrix job template.

use greenlit_workflow::model::job::Job;
use greenlit_workflow::model::workflow::Workflow;

use crate::graph::JobId;
use crate::matrix::MatrixLeg;
use crate::outputs::JobOutputsPlan;
use crate::partial_eval::{
    FoldCtx, LocatedEvalError, PartialEvalError, StaticRoots, StepsContextSlots, extend_env_chain,
};
use crate::pass_through::{ContainerPlan, EnvValue, plan_container, plan_env_layer};
use crate::planned::{Planned, plan_scalar_string, plan_template_string};
use crate::runner::{RunnerPlan, resolve_runs_on};

use super::error::retained_field_err;
use super::error::{eval_err, located_eval_err};
use super::job::PlanningState;
use super::step::plan_step;
use super::{PlanError, RetainedFieldError, RunDefaultsPlan, StepPlan};

pub(super) struct PlannedInstance {
    pub(super) name: Planned<String>,
    pub(super) runner: RunnerPlan,
    pub(super) container: Option<ContainerPlan>,
    pub(super) services: indexmap::IndexMap<String, ContainerPlan>,
    pub(super) env: indexmap::IndexMap<String, EnvValue>,
    pub(super) defaults: RunDefaultsPlan,
    pub(super) outputs: JobOutputsPlan,
    pub(super) steps: Vec<StepPlan>,
}

pub(super) fn plan_instance(
    workflow: &Workflow,
    job: &Job,
    job_id: &JobId,
    roots: StaticRoots<'_>,
    raw_outputs: &[(String, String, greenlit_workflow::Span)],
    leg: &MatrixLeg,
    state: &mut PlanningState<'_, '_>,
) -> Result<PlannedInstance, PlanError> {
    let workflow_env_chain = state.workflow_env;
    let job_layer_ctx = FoldCtx {
        roots,
        env: workflow_env_chain,
        secrets_forbidden: false,
    };
    let env = plan_env_layer(&job.env, &job_layer_ctx, state.size_budget)
        .map_err(|error| retained_field_err(job_id, None, error))?;

    // Reuse the already-evaluated workflow layer. Rebuilding it here, and
    // again for every step, multiplied large repository-authored env maps by
    // the step count and matrix-leg count without changing their context.
    let job_env_chain = extend_env_chain(workflow_env_chain, &job.env, roots, false)
        .map_err(|error| located_eval_err(job_id, None, error))?;
    let ctx = FoldCtx {
        roots,
        env: &job_env_chain,
        secrets_forbidden: false,
    };

    let runner = resolve_job_runner(job, job_id, &ctx)?;
    let runner_span = job
        .runs_on
        .as_ref()
        .map_or(&job.id.span, |runs_on| &runs_on.span);
    state.size_budget.add(&runner, runner_span)?;
    let default_name = format!("{}{}", job.id.value, leg.display_suffix());
    let name_span = job
        .name
        .as_ref()
        .map_or_else(|| job.id.span.clone(), |name| name.span.clone());
    let name = resolve_job_name(job, &default_name, &ctx)
        .map_err(|source| eval_err(job_id, None, name_span, source))?;
    state.size_budget.add(&name, &name.span)?;
    let container = job
        .container
        .as_ref()
        .map(|container| plan_container(&container.value, &ctx, state.size_budget))
        .transpose()
        .map_err(|error| retained_field_err(job_id, None, error))?;
    let mut services = indexmap::IndexMap::with_capacity(job.services.len());
    for (service_id, service) in &job.services {
        let service_plan = plan_container(&service.value, &ctx, state.size_budget)
            .map_err(|error| retained_field_err(job_id, None, error))?;
        state.size_budget.add(&service_id.value, &service_id.span)?;
        services.insert(service_id.value.clone(), service_plan);
    }
    let defaults = plan_defaults(workflow, job, &ctx, state.size_budget)
        .map_err(|error| retained_field_err(job_id, None, error))?;
    let mut steps = Vec::with_capacity(job.steps.len());
    let mut steps_slots = StepsContextSlots::default();
    let mut prior_step_can_mutate_env = false;
    for step in &job.steps {
        let step_roots = StaticRoots {
            steps_slots: &steps_slots,
            ..roots
        };
        let step_plan = plan_step(
            step,
            &step_roots,
            &job_env_chain,
            job_id,
            prior_step_can_mutate_env,
            state.size_budget,
        )?;
        if step_can_execute(&step_plan) {
            prior_step_can_mutate_env = true;
        }
        if let Some(step_id) = &step_plan.id {
            steps_slots.add(step_id.clone());
        }
        steps.push(step_plan);
    }

    // GitHub evaluates job outputs on the runner after all steps have
    // finished. Any step that can execute may have appended to GITHUB_ENV,
    // so outer environment lookups cannot be frozen to their declared value
    // at initial planning time.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idoutputs
    let output_env_chain = if prior_step_can_mutate_env {
        job_env_chain.after_executable_step()
    } else {
        job_env_chain
    };
    let output_roots = StaticRoots {
        steps_slots: &steps_slots,
        ..roots
    };
    let output_ctx = FoldCtx {
        roots: output_roots,
        env: &output_env_chain,
        secrets_forbidden: false,
    };
    let outputs = crate::outputs::plan_outputs(raw_outputs, &output_ctx, state.size_budget)
        .map_err(|error| retained_field_err(job_id, None, error))?;

    state.size_budget.add_bytes(192, &job.id.span)?;

    Ok(PlannedInstance {
        name,
        runner,
        container,
        services,
        env,
        defaults,
        outputs,
        steps,
    })
}

fn step_can_execute(step: &StepPlan) -> bool {
    !matches!(
        step.condition.as_ref().map(|condition| &condition.eval),
        Some(crate::condition::PlannedCond::Static(false))
    )
}

fn resolve_job_name(
    job: &Job,
    default_name: &str,
    ctx: &FoldCtx<'_>,
) -> Result<Planned<String>, PartialEvalError> {
    let Some(name) = &job.name else {
        return Ok(Planned::static_value(
            job.id.span.clone(),
            job.id.value.clone(),
            default_name.to_string(),
        ));
    };
    plan_template_string(&name.value, &name.span, ctx)
}

fn plan_defaults(
    workflow: &Workflow,
    job: &Job,
    ctx: &FoldCtx<'_>,
    size_budget: &mut super::PlanSizeBudget,
) -> Result<RunDefaultsPlan, RetainedFieldError> {
    let workflow_run = workflow
        .defaults
        .as_ref()
        .and_then(|defaults| defaults.value.run.as_ref());
    let job_run = job
        .defaults
        .as_ref()
        .and_then(|defaults| defaults.value.run.as_ref());

    let shell = job_run
        .and_then(|run| run.value.shell.as_ref())
        .or_else(|| workflow_run.and_then(|run| run.value.shell.as_ref()))
        .map(|shell| {
            plan_template_string(&shell.value, &shell.span, ctx).map_err(|source| {
                LocatedEvalError {
                    span: shell.span.clone(),
                    source,
                }
            })
        })
        .transpose()?;
    if let Some(planned) = &shell {
        size_budget.add(planned, &planned.span)?;
    }
    let working_directory = job_run
        .and_then(|run| run.value.working_directory.as_ref())
        .or_else(|| workflow_run.and_then(|run| run.value.working_directory.as_ref()))
        .map(|directory| {
            plan_scalar_string(directory, ctx).map_err(|source| LocatedEvalError {
                span: directory.span.clone(),
                source,
            })
        })
        .transpose()?;
    if let Some(planned) = &working_directory {
        size_budget.add(planned, &planned.span)?;
    }

    Ok(RunDefaultsPlan {
        shell,
        working_directory,
    })
}

fn resolve_job_runner(
    job: &Job,
    job_id: &JobId,
    ctx: &FoldCtx<'_>,
) -> Result<RunnerPlan, PlanError> {
    let runs_on = job
        .runs_on
        .as_ref()
        .ok_or_else(|| PlanError::NotSupportedInV0 {
            name: "reusable workflow call (jobs.<id>.uses)",
            span: job.id.span.clone(),
        })?;
    resolve_runs_on(runs_on, ctx).map_err(|source| PlanError::Runner {
        job: job_id.clone(),
        source: Box::new(source),
    })
}
