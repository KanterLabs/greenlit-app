//! Resolves all fields whose values can differ between concrete matrix
//! instances, or remain deferred on a dynamic-matrix job template.

use greenlit_workflow::model::job::Job;
use greenlit_workflow::model::value::ScalarOrExpr;
use greenlit_workflow::model::workflow::Workflow;

use crate::graph::JobId;
use crate::matrix::MatrixLeg;
use crate::outputs::JobOutputsPlan;
use crate::partial_eval::{
    FoldCtx, LocatedEvalError, PartialEvalError, StaticRoots, build_env_chain,
};
use crate::pass_through::{ContainerPlan, EnvValue, plan_container, plan_env_layer};
use crate::planned::{Planned, plan_scalar_string, plan_template_string};
use crate::runner::{RunnerPlan, resolve_runs_on};

use super::error::{eval_err, located_eval_err};
use super::step::plan_step;
use super::{PlanError, RunDefaultsPlan, StepPlan};

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
) -> Result<PlannedInstance, PlanError> {
    let workflow_env_layers: [&[(
        greenlit_workflow::Spanned<String>,
        greenlit_workflow::Spanned<ScalarOrExpr>,
    )]; 1] = [&workflow.env];
    let workflow_env_chain = build_env_chain(&workflow_env_layers, roots)
        .map_err(|error| located_eval_err(job_id, None, error))?;
    let job_layer_ctx = FoldCtx {
        roots,
        env: &workflow_env_chain,
        secrets_forbidden: false,
    };
    let env = plan_env_layer(&job.env, &job_layer_ctx)
        .map_err(|error| located_eval_err(job_id, None, error))?;

    let job_env_layers: [&[(
        greenlit_workflow::Spanned<String>,
        greenlit_workflow::Spanned<ScalarOrExpr>,
    )]; 2] = [&workflow.env, &job.env];
    let job_env_chain = build_env_chain(&job_env_layers, roots)
        .map_err(|error| located_eval_err(job_id, None, error))?;
    let ctx = FoldCtx {
        roots,
        env: &job_env_chain,
        secrets_forbidden: false,
    };

    let runner = resolve_job_runner(job, job_id, &ctx)?;
    let default_name = format!("{}{}", job.id.value, leg.display_suffix());
    let name_span = job
        .name
        .as_ref()
        .map_or_else(|| job.id.span.clone(), |name| name.span.clone());
    let name = resolve_job_name(job, &default_name, &ctx)
        .map_err(|source| eval_err(job_id, None, name_span, source))?;
    let container = job
        .container
        .as_ref()
        .map(|container| plan_container(&container.value, &ctx))
        .transpose()
        .map_err(|error| located_eval_err(job_id, None, error))?;
    let mut services = indexmap::IndexMap::with_capacity(job.services.len());
    for (service_id, service) in &job.services {
        let service_plan = plan_container(&service.value, &ctx)
            .map_err(|error| located_eval_err(job_id, None, error))?;
        services.insert(service_id.value.clone(), service_plan);
    }
    let defaults = plan_defaults(workflow, job, &ctx)
        .map_err(|error| located_eval_err(job_id, None, error))?;
    let outputs = crate::outputs::plan_outputs(raw_outputs, &ctx)
        .map_err(|error| located_eval_err(job_id, None, error))?;

    let mut steps = Vec::with_capacity(job.steps.len());
    for step in &job.steps {
        steps.push(plan_step(workflow, job, step, &roots, job_id)?);
    }

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
) -> Result<RunDefaultsPlan, LocatedEvalError> {
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
