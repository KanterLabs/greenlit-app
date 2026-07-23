//! Assembles one [`JobPlan`] (and its [`LegPlan`]s): resolves job `if:` once
//! before matrix expansion, then resolves `runs-on:`, display name,
//! `outputs:`, and every step independently for each concrete instance,
//! and validates `needs.*` references.

use greenlit_expr::Value;
use greenlit_workflow::model::job::Job;
use greenlit_workflow::model::workflow::Workflow;

use crate::event::SyntheticEvent;
use crate::graph::{JobGraph, JobId};
use crate::lints::Lint;
use crate::matrix::{MatrixLeg, plan_strategy};
use crate::outputs::JobOutputsPlan;
use crate::partial_eval::{
    EnvChain, FoldCtx, NeedsContextSlots, StaticRoots, StepsContextSlots, StrategyDeferred,
};
use crate::planned::Planned;

use super::budget::PlanSizeBudget;
use super::conditions::{expr_calls_status, fold_job_condition};
use super::contexts::{matrix_leg_value, strategy_context_value};
use super::instance::plan_instance;
use super::references::{ReferencingJob, WorkflowReferenceIndex, lint_needs_output_references};
use super::{JobPlan, LegPlan, PlanError, PlanOptions, RunDefaultsPlan};

pub(super) struct PlanningState<'state, 'env> {
    pub(super) lints: &'state mut Vec<Lint>,
    pub(super) size_budget: &'state mut PlanSizeBudget,
    pub(super) workflow_env: &'env EnvChain,
    pub(super) references: &'env WorkflowReferenceIndex<'env>,
    pub(super) job_index: usize,
}

pub(crate) fn plan_job(
    workflow: &Workflow,
    job: &Job,
    event: &SyntheticEvent,
    options: &PlanOptions,
    graph: &JobGraph,
    state: &mut PlanningState<'_, '_>,
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
    let needs_slots = NeedsContextSlots::new(
        needs
            .iter()
            .filter_map(|need| {
                graph.idx_of(need).and_then(|dependency_index| {
                    state
                        .references
                        .output_names(dependency_index.0 as usize)
                        .map(|outputs| (need.clone(), outputs))
                })
            })
            .collect(),
    );
    let needs_context = needs_slots.static_value();

    let roots_null = Value::Null;
    let empty_context = Value::object(vec![]);
    let empty_steps_slots = StepsContextSlots::default();
    let static_roots = StaticRoots {
        github: &event.github,
        github_deferred: &event.deferred_github_properties,
        vars: &options.vars,
        needs: &needs_context,
        needs_slots: &needs_slots,
        matrix: &roots_null,
        matrix_deferred: false,
        strategy: &roots_null,
        strategy_deferred: StrategyDeferred::default(),
        steps: &empty_context,
        steps_slots: &empty_steps_slots,
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
    state.lints.append(&mut strategy_lints);
    let strategy_span = job
        .strategy
        .as_ref()
        .map_or(&job.id.span, |strategy| &strategy.span);
    state.size_budget.add(&strategy, strategy_span)?;

    let raw_outputs: Vec<(String, String, greenlit_workflow::Span)> = job
        .outputs
        .iter()
        .map(|(k, v)| (k.value.clone(), v.value.clone(), v.span.clone()))
        .collect();

    // The matrix context changes for every generated job. Accordingly,
    // every context-sensitive field below is planned independently for
    // each static leg. When expansion depends on `needs` output data, the
    // same fields form one explicit deferred job template: `matrix.*` and
    // `strategy.*` references remain residuals until Phase 2 materializes
    // its concrete legs after the prerequisites finish.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#matrix-context
    let (runner, name, container, services, env, defaults, outputs, steps, legs) =
        if strategy.is_matrix_deferred() {
            let deferred_strategy = super::contexts::deferred_strategy_context_value(&strategy);
            let deferred_roots = StaticRoots {
                matrix_deferred: true,
                strategy: &deferred_strategy,
                strategy_deferred: deferred_strategy_fields(&strategy, true),
                ..static_roots
            };
            let template_leg = MatrixLeg {
                index: 0,
                values: indexmap::IndexMap::new(),
                origin: crate::matrix::LegOrigin::Product,
            };
            let instance = plan_instance(
                workflow,
                job,
                &job_id,
                deferred_roots,
                &raw_outputs,
                &template_leg,
                state,
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
        } else if strategy.is_matrix() {
            let mut legs = Vec::with_capacity(strategy.legs().len());
            for leg in strategy.legs() {
                let leg_matrix = matrix_leg_value(leg);
                let leg_strategy = strategy_context_value(&strategy, leg.index);
                let leg_roots = StaticRoots {
                    matrix: &leg_matrix,
                    strategy: &leg_strategy,
                    strategy_deferred: deferred_strategy_fields(&strategy, false),
                    ..static_roots
                };
                let instance =
                    plan_instance(workflow, job, &job_id, leg_roots, &raw_outputs, leg, state)?;
                // Reserve the cloned condition and leg envelope before the
                // clone joins the retained aggregate.
                state.size_budget.add(&condition, &job.id.span)?;
                state.size_budget.add_bytes(160, &job.id.span)?;
                let leg_condition = condition.clone();
                let leg_plan = LegPlan {
                    name: instance.name,
                    runner: instance.runner,
                    container: instance.container,
                    services: instance.services,
                    env: instance.env,
                    defaults: instance.defaults,
                    condition: leg_condition,
                    skip: None,
                    outputs: instance.outputs,
                    steps: instance.steps,
                };
                legs.push(leg_plan);
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
                state,
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

    let is_static_matrix = strategy.is_matrix() && !strategy.is_matrix_deferred();
    // All potentially large instance fields were charged individually.
    // Reserve the remaining metadata and envelope before assembling the
    // complete retained job.
    state.size_budget.add(&job_id, &job.id.span)?;
    state.size_budget.add(&needs, &job.id.span)?;
    if is_static_matrix {
        state.size_budget.add(&name, &job.id.span)?;
    } else {
        state.size_budget.add(&condition, &job.id.span)?;
    }
    state.size_budget.add_bytes(320, &job.id.span)?;

    let plan = JobPlan {
        id: job_id,
        span: job.id.span.clone(),
        name,
        name_is_default: job.name.is_none(),
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
        permissions: None,
        condition: if strategy.is_matrix() && !strategy.is_matrix_deferred() {
            None
        } else {
            condition
        },
        implicit_status_gate,
        skip: None,
        strategy,
        legs,
        outputs,
        steps,
    };

    lint_needs_output_references(
        &ReferencingJob { needs: &plan.needs },
        state.job_index,
        state.references,
        graph,
        state.lints,
    );

    Ok(plan)
}

fn deferred_strategy_fields(
    strategy: &crate::matrix::StrategyPlan,
    matrix_pending: bool,
) -> StrategyDeferred {
    StrategyDeferred {
        fail_fast: strategy.fail_fast.is_deferred(),
        job_index: matrix_pending,
        job_total: matrix_pending,
        max_parallel: match strategy.max_parallel.as_ref() {
            None => matrix_pending,
            Some(value) => value.is_deferred(),
        },
    }
}
