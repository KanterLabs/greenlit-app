//! Assembles one [`JobPlan`] (and its [`LegPlan`]s): resolves job `if:` once
//! before matrix expansion, then resolves `runs-on:`, display name,
//! `outputs:`, and every step independently for each concrete instance,
//! and validates `needs.*` references.

use greenlit_expr::Value;
use greenlit_workflow::model::job::Job;
use greenlit_workflow::model::value::ScalarOrExpr;
use greenlit_workflow::model::workflow::Workflow;

use crate::event::SyntheticEvent;
use crate::graph::{JobGraph, JobId};
use crate::lints::Lint;
use crate::matrix::{MatrixLeg, plan_strategy};
use crate::outputs::JobOutputsPlan;
use crate::partial_eval::{
    EnvChain, FoldCtx, LocatedEvalError, PartialEvalError, StaticRoots, build_env_chain,
};
use crate::pass_through::{plan_container, plan_env_layer};
use crate::planned::{Planned, plan_scalar_string, plan_template_string};
use crate::runner::{RunnerPlan, resolve_runs_on};

use super::conditions::{expr_calls_status, fold_job_condition};
use super::contexts::{matrix_leg_value, strategy_context_value};
use super::error::{eval_err, located_eval_err};
use super::references::{ReferencingJob, lint_needs_output_references};
use super::step::plan_step;
use super::{JobPlan, LegPlan, PlanError, PlanOptions, RunDefaultsPlan, StepPlan};

pub(crate) fn plan_job(
    workflow: &Workflow,
    job: &Job,
    event: &SyntheticEvent,
    options: &PlanOptions,
    graph: &JobGraph,
    lints: &mut Vec<Lint>,
) -> Result<JobPlan, PlanError> {
    let job_id = JobId(job.id.value.clone());
    let needs: Vec<JobId> = {
        let mut seen = std::collections::HashSet::new();
        job.needs
            .iter()
            .filter(|n| seen.insert(n.value.clone()))
            .map(|n| JobId(n.value.clone()))
            .collect()
    };

    let roots_null = Value::Null;
    let static_roots = StaticRoots {
        github: &event.github,
        vars: &options.vars,
        needs: None,
        matrix: &roots_null,
        matrix_deferred: false,
        strategy: &roots_null,
        strategy_deferred: false,
        inputs: &event.inputs,
    };

    let empty_env = EnvChain::empty();
    let job_ctx = FoldCtx {
        roots: static_roots,
        env: &empty_env,
        secrets_forbidden: true,
    };

    // GitHub workflow syntax, `jobs.<job_id>.if`: this condition is
    // evaluated before `strategy.matrix` is applied. The Context
    // availability table limits it to github/needs/vars/inputs (plus the
    // four status functions), so it is planned exactly once with no
    // matrix or env context and then copied to concrete legs.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idif
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability
    let condition = fold_job_condition(job, &job_ctx, &job_id)?;
    let implicit_status_gate = match &job.if_condition {
        None => true,
        Some(raw) => !expr_calls_status(&raw.value),
    };

    let matrix_ctx_for_strategy = FoldCtx {
        roots: static_roots,
        env: &empty_env,
        secrets_forbidden: false,
    };

    let (strategy, mut strategy_lints) = plan_strategy(
        job.strategy.as_ref().map(|s| &s.value),
        &matrix_ctx_for_strategy,
        options.max_matrix_legs,
    )
    .map_err(|source| PlanError::Matrix {
        job: job_id.clone(),
        source: Box::new(source),
    })?;
    lints.append(&mut strategy_lints);

    let raw_outputs: Vec<(String, String, greenlit_workflow::Span)> = job
        .outputs
        .iter()
        .map(|(k, v)| (k.value.clone(), v.value.clone(), v.span.clone()))
        .collect();

    // The matrix context changes for every generated job. Accordingly,
    // every context-sensitive field below is planned independently for
    // each leg: runner, display name, outputs, and the complete step list.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#matrix-context
    let (runner, name, container, services, env, defaults, outputs, steps, legs) =
        if strategy.is_matrix {
            let mut legs = Vec::with_capacity(strategy.legs.len());
            for leg in &strategy.legs {
                let leg_matrix = matrix_leg_value(leg);
                let leg_strategy = strategy_context_value(&strategy, leg.index);
                let leg_roots = StaticRoots {
                    matrix: &leg_matrix,
                    strategy: &leg_strategy,
                    ..static_roots
                };
                let instance = plan_instance(workflow, job, &job_id, leg_roots, &raw_outputs, leg)?;
                legs.push(LegPlan {
                    name: instance.name,
                    runner: instance.runner,
                    container: instance.container,
                    services: instance.services,
                    env: instance.env,
                    defaults: instance.defaults,
                    condition: condition.clone(),
                    skip: None,
                    outputs: instance.outputs,
                    steps: instance.steps,
                });
            }
            (
                None,
                Planned::static_value(
                    job.id.span.clone(),
                    job.id.value.clone(),
                    job.id.value.clone(),
                ),
                None,
                indexmap::IndexMap::new(),
                indexmap::IndexMap::new(),
                RunDefaultsPlan::default(),
                JobOutputsPlan::default(),
                Vec::new(),
                legs,
            )
        } else {
            let implicit_leg = MatrixLeg {
                index: 0,
                values: indexmap::IndexMap::new(),
                origin: crate::matrix::LegOrigin::Product,
            };
            let instance = plan_instance(
                workflow,
                job,
                &job_id,
                static_roots,
                &raw_outputs,
                &implicit_leg,
            )?;
            (
                Some(instance.runner),
                instance.name,
                instance.container,
                instance.services,
                instance.env,
                instance.defaults,
                instance.outputs,
                instance.steps,
                Vec::new(),
            )
        };

    let plan = JobPlan {
        id: job_id,
        span: job.id.span.clone(),
        name,
        needs,
        wave: graph
            .idx_of(&JobId(job.id.value.clone()))
            .map(|idx| graph.wave(idx))
            .unwrap_or(0),
        runner,
        container,
        services,
        env,
        defaults,
        condition: if strategy.is_matrix { None } else { condition },
        implicit_status_gate,
        skip: None,
        strategy,
        legs,
        outputs,
        steps,
    };

    lint_needs_output_references(
        &ReferencingJob {
            span: &job.id.span,
            needs: &plan.needs,
        },
        &plan,
        workflow,
        lints,
    );

    Ok(plan)
}

struct PlannedInstance {
    name: Planned<String>,
    runner: RunnerPlan,
    container: Option<crate::pass_through::ContainerPlan>,
    services: indexmap::IndexMap<String, crate::pass_through::ContainerPlan>,
    env: indexmap::IndexMap<String, crate::pass_through::EnvValue>,
    defaults: RunDefaultsPlan,
    outputs: JobOutputsPlan,
    steps: Vec<StepPlan>,
}

fn plan_instance(
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
