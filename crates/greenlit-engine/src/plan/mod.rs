//! [`ExecutionPlan`]: the fully resolved plan `litci plan` prints, and
//! [`plan()`], the crate's top-level entrypoint.
//!
//! Planning is split by concern: workflow orchestration here, one job in
//! `job`, one step in `step`, skip propagation in `skip`, and the stable
//! public contract/error types in `types` and `error`.

mod budget;
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
pub(crate) use error::RetainedFieldError;
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
use crate::evidence::{FeatureFinding, FindingDisposition, SupportReport};
use crate::graph::{JobId, build_graph};
use crate::lints::Lint;
use crate::partial_eval::{
    EnvChain, FoldCtx, NeedsContextSlots, StaticRoots, StepsContextSlots, StrategyDeferred,
    build_env_chain,
};
use crate::pass_through::plan_env_layer;
use crate::planned::{Evaluation, plan_scalar_string, plan_template_string};
pub(crate) use budget::PlanSizeBudget;

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
    validate_v0_support(workflow)?;
    let static_extraction = greenlit_workflow::extract_static(workflow).map_err(|source| {
        PlanError::StaticExtraction {
            source: Box::new(source),
        }
    })?;

    let null = Value::Null;
    let empty_context = Value::object(vec![]);
    let empty_needs_slots = NeedsContextSlots::default();
    let empty_steps_slots = StepsContextSlots::default();
    let roots = StaticRoots {
        github: &event.github,
        github_deferred: &event.deferred_github_properties,
        vars: &options.vars,
        needs: &empty_context,
        needs_slots: &empty_needs_slots,
        matrix: &null,
        matrix_deferred: false,
        strategy: &null,
        strategy_deferred: StrategyDeferred::default(),
        steps: &empty_context,
        steps_slots: &empty_steps_slots,
        inputs: &event.inputs,
    };
    let empty_env = EnvChain::empty();
    let workflow_ctx = FoldCtx {
        roots,
        env: &empty_env,
        secrets_forbidden: false,
    };
    let mut size_budget = PlanSizeBudget::new();
    let run_name = workflow
        .run_name
        .as_ref()
        .map(|run_name| {
            plan_template_string(&run_name.value, &run_name.span, &workflow_ctx).map_err(|source| {
                PlanError::WorkflowEval {
                    span: run_name.span.clone(),
                    source: Box::new(source),
                }
            })
        })
        .transpose()?
        // GitHub falls back to event-specific run information when the
        // resolved run name contains only whitespace.
        // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#run-name
        .filter(|planned| {
            !matches!(&planned.evaluation, Evaluation::Static(value) if value.trim().is_empty())
        });
    if let Some(planned) = &run_name {
        size_budget.add(planned, &planned.span)?;
    }
    let env = plan_env_layer(&workflow.env, &workflow_ctx, &mut size_budget)
        .map_err(workflow_retained_error)?;
    // Workflow-level env may use only workflow-invariant roots. Build its
    // lookup chain once and share it across every job and matrix leg.
    let workflow_env_chain =
        build_env_chain(&[workflow.env.as_slice()], roots).map_err(|error| {
            PlanError::WorkflowEval {
                span: error.span,
                source: Box::new(error.source),
            }
        })?;
    let defaults = plan_workflow_defaults(workflow, &workflow_ctx, &mut size_budget)?;
    let permissions = workflow.permissions.as_ref().map(plan_permissions);
    let permissions_span = workflow
        .permissions
        .as_ref()
        .map_or(&workflow.span, |permissions| &permissions.span);
    size_budget.add(&permissions, permissions_span)?;

    let graph = build_graph(&workflow.jobs)?;
    let reference_index =
        references::WorkflowReferenceIndex::new(workflow, &static_extraction.needs_outputs);

    let mut lints = Vec::new();
    let mut job_plans: Vec<JobPlan> = Vec::with_capacity(workflow.jobs.len());
    for (job_index, job) in workflow.jobs.iter().enumerate() {
        let effective_permissions = job.permissions.as_ref().or(workflow.permissions.as_ref());
        lint_duplicate_needs(job, &mut lints);
        let mut planning_state = job::PlanningState {
            lints: &mut lints,
            size_budget: &mut size_budget,
            workflow_env: &workflow_env_chain,
            references: &reference_index,
            job_index,
        };
        let mut job_plan =
            job::plan_job(workflow, job, event, options, &graph, &mut planning_state)?;
        // GitHub documents job-level permissions as a complete replacement
        // for the workflow declaration for that job. Within a scoped
        // declaration, every omitted scope becomes `none`.
        // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idpermissions
        let effective_permissions = effective_permissions.map(plan_permissions);
        let effective_permissions_span = job
            .permissions
            .as_ref()
            .or(workflow.permissions.as_ref())
            .map_or(&job.id.span, |permissions| &permissions.span);
        size_budget.add(&effective_permissions, effective_permissions_span)?;
        job_plan.permissions = effective_permissions;
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

    for job in &job_plans {
        size_budget.add(&job.skip, &job.span)?;
        for leg in &job.legs {
            size_budget.add(&leg.skip, &job.span)?;
        }
    }
    size_budget.add(&lints, &workflow.span)?;

    let topo_order: Vec<JobId> = graph
        .topo_order()
        .iter()
        .map(|idx| graph.id_of(*idx).clone())
        .collect();
    size_budget.add(&topo_order, &workflow.span)?;
    let event_name = event.kind.event_name().to_string();
    size_budget.add(&event_name, &workflow.span)?;
    size_budget.add_bytes(512, &workflow.span)?;

    let execution_plan = ExecutionPlan {
        schema_version: 1,
        event_name,
        run_name,
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

/// Rejects every parsed construct that Greenlit deliberately excludes from
/// v0, without requiring an event, local variables, or any other planning
/// input.
///
/// CLI callers invoke this immediately after workflow parsing so a missing
/// variable or mismatched synthetic event cannot obscure the authored
/// unsupported construct. [`plan`] invokes it again to keep the library
/// entrypoint independently safe and complete.
pub fn validate_v0_support(workflow: &Workflow) -> Result<(), PlanError> {
    reject_unsupported_workflow_constructs(workflow)?;
    for job in &workflow.jobs {
        reject_unsupported_job_constructs(job)?;
        let effective_permissions = job.permissions.as_ref().or(workflow.permissions.as_ref());
        reject_oidc_permissions(effective_permissions)?;
    }
    Ok(())
}

/// Inventories recognized workflow features whose semantics Greenlit cannot
/// execute faithfully. The report is independent from policy: callers can
/// persist it before [`validate_v0_support`] blocks execution.
#[must_use]
pub fn analyze_support(workflow: &Workflow) -> SupportReport {
    let mut findings = Vec::new();
    if workflow.concurrency.is_some() {
        findings.push(unsupported_finding(
            "workflow.concurrency",
            "workflow",
            "workflow-level concurrency groups are not implemented",
        ));
    }
    for trigger in &workflow.on {
        if matches!(
            trigger.value,
            greenlit_workflow::model::trigger::Trigger::WorkflowCall(_)
        ) {
            findings.push(unsupported_finding(
                "workflow.reusable_trigger",
                "workflow.on.workflow_call",
                "reusable workflow triggers are not implemented",
            ));
        }
    }
    for job in &workflow.jobs {
        let scope = format!("jobs.{}", job.id.value);
        if job.environment.is_some() {
            findings.push(unsupported_finding(
                "job.environment",
                &scope,
                "GitHub environment protection and deployments are unavailable locally",
            ));
        }
        if job.concurrency.is_some() {
            findings.push(unsupported_finding(
                "job.concurrency",
                &scope,
                "job-level concurrency groups are not implemented",
            ));
        }
        if job.reusable_call.is_some() {
            findings.push(unsupported_finding(
                "job.reusable_workflow",
                &scope,
                "reusable workflow call jobs are not implemented",
            ));
        }
        let effective_permissions = job.permissions.as_ref().or(workflow.permissions.as_ref());
        if permissions_request_oidc(effective_permissions) {
            findings.push(unsupported_finding(
                "github.oidc",
                &scope,
                "GitHub OIDC token issuance is unavailable locally",
            ));
        }
    }
    let mut report = SupportReport { findings };
    report.canonicalize();
    report
}

fn unsupported_finding(code: &str, scope: &str, reason: &str) -> FeatureFinding {
    FeatureFinding {
        code: code.to_string(),
        disposition: FindingDisposition::Unsupported,
        scope: scope.to_string(),
        reason: reason.to_string(),
    }
}

fn permissions_request_oidc(permissions: Option<&greenlit_workflow::Spanned<Permissions>>) -> bool {
    permissions.is_some_and(|permissions| match &permissions.value {
        Permissions::All(PermissionLevelAll::WriteAll) => true,
        Permissions::All(PermissionLevelAll::ReadAll) => false,
        Permissions::Scoped(entries) => entries.iter().any(|(scope, level)| {
            scope.value == "id-token" && level.value == PermissionLevel::Write
        }),
    })
}

fn plan_workflow_defaults(
    workflow: &Workflow,
    ctx: &FoldCtx<'_>,
    size_budget: &mut PlanSizeBudget,
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
    if let Some(planned) = &shell {
        size_budget.add(planned, &planned.span)?;
    }
    let working_directory = run
        .and_then(|run| run.value.working_directory.as_ref())
        .map(|directory| {
            plan_scalar_string(directory, ctx).map_err(|source| PlanError::WorkflowEval {
                span: directory.span.clone(),
                source: Box::new(source),
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

fn workflow_retained_error(error: RetainedFieldError) -> PlanError {
    match error {
        RetainedFieldError::Evaluation(error) => PlanError::WorkflowEval {
            span: error.span,
            source: Box::new(error.source),
        },
        RetainedFieldError::Limit(error) => error,
    }
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

fn reject_oidc_permissions(
    permissions: Option<&greenlit_workflow::Spanned<Permissions>>,
) -> Result<(), PlanError> {
    let Some(permissions) = permissions else {
        return Ok(());
    };
    let oidc_span = match &permissions.value {
        // `write-all` grants every available write permission, including
        // `id-token: write`. Greenlit v0 deliberately has no OIDC provider.
        // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#permissions
        Permissions::All(PermissionLevelAll::WriteAll) => Some(&permissions.span),
        Permissions::All(PermissionLevelAll::ReadAll) => None,
        Permissions::Scoped(entries) => entries.iter().find_map(|(scope, level)| {
            (scope.value == "id-token" && level.value == PermissionLevel::Write)
                .then_some(&level.span)
        }),
    };
    match oidc_span {
        Some(span) => Err(PlanError::NotSupportedInV0 {
            name: "OIDC (`permissions: id-token: write`)",
            span: span.clone(),
        }),
        None => Ok(()),
    }
}

fn lint_duplicate_needs(job: &Job, lints: &mut Vec<Lint>) {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for need in &job.needs {
        if !seen.insert(&need.value) {
            lints.push(Lint::duplicate_needs(need.span.clone(), &need.value));
        }
    }
}
