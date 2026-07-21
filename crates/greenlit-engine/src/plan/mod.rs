//! [`ExecutionPlan`]: the fully resolved plan `litci plan` prints, and
//! [`plan()`], the crate's top-level entrypoint.
//!
//! Planning is split by concern: workflow orchestration here, one job in
//! `job`, one step in `step`, skip propagation in `skip`, and the stable
//! public contract/error types in `types` and `error`.

mod conditions;
mod contexts;
mod error;
mod instance;
mod job;
mod references;
mod skip;
mod step;
mod types;

pub use error::PlanError;
pub use types::{
    ExecutionPlan, JobPlan, LegPlan, PermissionLevelPlan, PermissionsPlan, PlanOptions,
    RunDefaultsPlan, StaticSkip, StepKind, StepPlan,
};

use greenlit_expr::Value;
use greenlit_workflow::model::job::Job;
use greenlit_workflow::model::workflow::{
    PermissionLevel, PermissionLevelAll, Permissions, Workflow,
};

use crate::event::SyntheticEvent;
use crate::graph::{JobId, build_graph};
use crate::lints::Lint;
use crate::partial_eval::{EnvChain, FoldCtx, StaticRoots, StrategyDeferred};
use crate::pass_through::plan_env_layer;
use crate::planned::{plan_scalar_string, plan_template_string};

/// Plans `workflow` against `event`, using `options` for local variable
/// overrides. The single entrypoint `greenlit-app` calls.
pub fn plan(
    workflow: &Workflow,
    event: &SyntheticEvent,
    options: &PlanOptions,
) -> Result<ExecutionPlan, PlanError> {
    let evaluation_span = tracing::info_span!(
        target: "greenlit_metrics::timed_stage",
        "greenlit_stage",
        stage = "eval"
    );
    let evaluation_guard = evaluation_span.enter();
    reject_unsupported_workflow_constructs(workflow)?;

    let null = Value::Null;
    let roots = StaticRoots {
        github: &event.github,
        vars: &options.vars,
        needs: None,
        matrix: &null,
        matrix_deferred: false,
        strategy: &null,
        strategy_deferred: StrategyDeferred::default(),
        inputs: &event.inputs,
    };
    let empty_env = EnvChain::empty();
    let workflow_ctx = FoldCtx {
        roots,
        env: &empty_env,
        secrets_forbidden: false,
    };
    let env =
        plan_env_layer(&workflow.env, &workflow_ctx).map_err(|error| PlanError::WorkflowEval {
            span: error.span,
            source: Box::new(error.source),
        })?;
    let defaults = plan_workflow_defaults(workflow, &workflow_ctx)?;
    let permissions = workflow.permissions.as_ref().map(plan_permissions);

    let graph = build_graph(&workflow.jobs)?;

    let mut lints = Vec::new();
    let mut job_plans: Vec<JobPlan> = Vec::with_capacity(workflow.jobs.len());
    for job in &workflow.jobs {
        reject_unsupported_job_constructs(job)?;
        lint_duplicate_needs(job, &mut lints);
        let job_plan = job::plan_job(workflow, job, event, options, &graph, &mut lints)?;
        job_plans.push(job_plan);
    }

    drop(evaluation_guard);
    drop(evaluation_span);
    let assembly_span = tracing::info_span!(
        target: "greenlit_metrics::timed_stage",
        "greenlit_stage",
        stage = "plan"
    );
    let assembly_guard = assembly_span.enter();

    skip::propagate_static_skip(&graph, &mut job_plans);
    references::lint_matrix_output_collisions(&job_plans, &mut lints);

    let topo_order: Vec<JobId> = graph
        .topo_order()
        .iter()
        .map(|idx| graph.id_of(*idx).clone())
        .collect();

    let execution_plan = ExecutionPlan {
        schema_version: 1,
        event_name: event.kind.event_name().to_string(),
        env,
        defaults,
        permissions,
        jobs: job_plans,
        topo_order,
        lints,
    };
    drop(assembly_guard);
    drop(assembly_span);
    Ok(execution_plan)
}

fn plan_workflow_defaults(
    workflow: &Workflow,
    ctx: &FoldCtx<'_>,
) -> Result<RunDefaultsPlan, PlanError> {
    let run = workflow
        .defaults
        .as_ref()
        .and_then(|defaults| defaults.value.run.as_ref());
    let shell = run
        .and_then(|run| run.value.shell.as_ref())
        .map(|shell| {
            plan_template_string(&shell.value, &shell.span, ctx).map_err(|source| {
                PlanError::WorkflowEval {
                    span: shell.span.clone(),
                    source: Box::new(source),
                }
            })
        })
        .transpose()?;
    let working_directory = run
        .and_then(|run| run.value.working_directory.as_ref())
        .map(|directory| {
            plan_scalar_string(directory, ctx).map_err(|source| PlanError::WorkflowEval {
                span: directory.span.clone(),
                source: Box::new(source),
            })
        })
        .transpose()?;
    Ok(RunDefaultsPlan {
        shell,
        working_directory,
    })
}

fn plan_permissions(permissions: &greenlit_workflow::Spanned<Permissions>) -> PermissionsPlan {
    match &permissions.value {
        Permissions::All(PermissionLevelAll::ReadAll) => PermissionsPlan::ReadAll,
        Permissions::All(PermissionLevelAll::WriteAll) => PermissionsPlan::WriteAll,
        Permissions::Scoped(entries) => PermissionsPlan::Scoped {
            scopes: entries
                .iter()
                .map(|(scope, level)| {
                    let level = match level.value {
                        PermissionLevel::Read => PermissionLevelPlan::Read,
                        PermissionLevel::Write => PermissionLevelPlan::Write,
                        PermissionLevel::None => PermissionLevelPlan::None,
                    };
                    (scope.value.clone(), level)
                })
                .collect(),
        },
    }
}

fn reject_unsupported_workflow_constructs(workflow: &Workflow) -> Result<(), PlanError> {
    if let Some(uc) = &workflow.concurrency {
        return Err(PlanError::NotSupportedInV0 {
            name: uc.name,
            span: uc.location.clone(),
        });
    }
    for trigger in &workflow.on {
        if let greenlit_workflow::model::trigger::Trigger::WorkflowCall(uc) = &trigger.value {
            return Err(PlanError::NotSupportedInV0 {
                name: uc.name,
                span: uc.location.clone(),
            });
        }
    }
    Ok(())
}

fn reject_unsupported_job_constructs(job: &Job) -> Result<(), PlanError> {
    if let Some(uc) = [&job.environment, &job.concurrency, &job.reusable_call]
        .into_iter()
        .flatten()
        .next()
    {
        return Err(PlanError::NotSupportedInV0 {
            name: uc.name,
            span: uc.location.clone(),
        });
    }
    Ok(())
}

fn lint_duplicate_needs(job: &Job, lints: &mut Vec<Lint>) {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for need in &job.needs {
        if !seen.insert(&need.value) {
            lints.push(Lint::duplicate_needs(need.span.clone(), &need.value));
        }
    }
}
