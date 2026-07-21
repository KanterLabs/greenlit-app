//! Assembles one [`JobPlan`] (and its [`LegPlan`]s): resolves job `if:` once
//! before matrix expansion, then resolves `runs-on:`, display name,
//! `outputs:`, and every step independently for each concrete instance,
//! and validates `needs.*` references.

use greenlit_expr::value::to_display_string;
use greenlit_expr::{Expr, Value};
use greenlit_workflow::model::job::Job;
use greenlit_workflow::model::value::ScalarOrExpr;
use greenlit_workflow::model::workflow::Workflow;

use crate::condition::{Condition, plan_condition};
use crate::defer::DeferReason;
use crate::event::SyntheticEvent;
use crate::graph::{JobGraph, JobId};
use crate::lints::Lint;
use crate::matrix::{MatrixLeg, MatrixValue, plan_strategy};
use crate::outputs::JobOutputsPlan;
use crate::partial_eval::{
    EnvChain, FoldCtx, PartialEvalError, StaticRoots, TemplateFold, build_env_chain, fold_template,
};
use crate::pass_through::{container_plan_from, env_layer_to_map};
use crate::runner::{RunnerImage, resolve_runs_on};

use super::step::plan_step;
use super::{JobPlan, LegPlan, PlanError, PlanOptions, StepPlan};

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
        matrix: &roots_null,
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

    let raw_outputs: Vec<(String, String)> = job
        .outputs
        .iter()
        .map(|(k, v)| (k.value.clone(), v.value.clone()))
        .collect();

    // The matrix context changes for every generated job. Accordingly,
    // every context-sensitive field below is planned independently for
    // each leg: runner, display name, outputs, and the complete step list.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#matrix-context
    let (runner, name, outputs, steps, legs) = if strategy.is_matrix {
        let mut legs = Vec::with_capacity(strategy.legs.len());
        for leg in &strategy.legs {
            let leg_matrix = matrix_leg_value(leg);
            let leg_roots = StaticRoots {
                matrix: &leg_matrix,
                ..static_roots
            };
            let instance = plan_instance(workflow, job, &job_id, leg_roots, &raw_outputs, leg)?;
            legs.push(LegPlan {
                name: instance.name,
                runner: instance.runner,
                condition: condition.clone(),
                skip: None,
                outputs: instance.outputs,
                steps: instance.steps,
            });
        }
        (
            None,
            job.id.value.clone(),
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
            instance.outputs,
            instance.steps,
            Vec::new(),
        )
    };

    validate_needs_references(
        &ReferencingJob {
            id: &job_id,
            span: &job.id.span,
            needs: &needs,
        },
        &JobPlanParts {
            condition: &condition,
            legs: &legs,
            steps: &steps,
            outputs: &outputs,
        },
        workflow,
        lints,
    )?;

    Ok(JobPlan {
        id: job_id,
        span: job.id.span.clone(),
        name,
        needs,
        wave: graph
            .idx_of(&JobId(job.id.value.clone()))
            .map(|idx| graph.wave(idx))
            .unwrap_or(0),
        runner,
        container: job
            .container
            .as_ref()
            .map(|c| container_plan_from(&c.value)),
        env: env_layer_to_map(&job.env),
        condition: if strategy.is_matrix { None } else { condition },
        implicit_status_gate,
        skip: None,
        strategy,
        legs,
        outputs,
        steps,
    })
}

struct PlannedInstance {
    name: String,
    runner: RunnerImage,
    outputs: JobOutputsPlan,
    steps: Vec<StepPlan>,
}

fn plan_instance(
    workflow: &Workflow,
    job: &Job,
    job_id: &JobId,
    roots: StaticRoots<'_>,
    raw_outputs: &[(String, String)],
    leg: &MatrixLeg,
) -> Result<PlannedInstance, PlanError> {
    let job_env_layers: [&[(
        greenlit_workflow::Spanned<String>,
        greenlit_workflow::Spanned<ScalarOrExpr>,
    )]; 2] = [&workflow.env, &job.env];
    let job_env_chain =
        build_env_chain(&job_env_layers, roots).map_err(|source| eval_err(job_id, None, source))?;
    let ctx = FoldCtx {
        roots,
        env: &job_env_chain,
        secrets_forbidden: false,
    };

    let runner = resolve_job_runner(job, job_id, &ctx)?;
    let default_name = format!("{}{}", job.id.value, leg.display_suffix());
    let name = resolve_job_name(job, &default_name, &ctx)
        .map_err(|source| eval_err(job_id, None, source))?;
    let outputs = crate::outputs::plan_outputs(raw_outputs, &ctx)
        .map_err(|source| eval_err(job_id, None, source))?;

    let mut steps = Vec::with_capacity(job.steps.len());
    for step in &job.steps {
        steps.push(plan_step(workflow, job, step, &roots, job_id)?);
    }

    Ok(PlannedInstance {
        name,
        runner,
        outputs,
        steps,
    })
}

fn resolve_job_name(
    job: &Job,
    default_name: &str,
    ctx: &FoldCtx<'_>,
) -> Result<String, PartialEvalError> {
    let Some(name) = &job.name else {
        return Ok(default_name.to_string());
    };
    match fold_template(&name.value, ctx)? {
        TemplateFold::Static(value) => Ok(to_display_string(&value)),
        TemplateFold::Deferred { residual_text, .. } => Ok(residual_text),
    }
}

fn fold_job_condition(
    job: &Job,
    ctx: &FoldCtx<'_>,
    job_id: &JobId,
) -> Result<Option<Condition>, PlanError> {
    let Some(raw) = &job.if_condition else {
        return Ok(None);
    };
    validate_job_condition_availability(&raw.value, &raw.span, job_id)?;
    plan_condition(&raw.value, ctx)
        .map(Some)
        .map_err(|source| eval_err(job_id, None, source))
}

/// GitHub's Context availability table is authoritative for this workflow
/// key: `jobs.<job_id>.if` may use only `github`, `needs`, `vars`, and
/// `inputs`; among restricted functions it may use only the four status
/// functions. Treating a disallowed root as an empty object would silently
/// turn an invalid GitHub workflow into a plausible local plan.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability
fn validate_job_condition_availability(
    raw: &str,
    span: &greenlit_workflow::Span,
    job_id: &JobId,
) -> Result<(), PlanError> {
    let Ok(expr) = greenlit_expr::parse(strip_wrapper(raw)) else {
        // The normal condition parser below reports the richer parse error.
        return Ok(());
    };
    let Some(unavailable) = first_unavailable_job_condition_reference(&expr) else {
        return Ok(());
    };
    Err(PlanError::JobConditionUnavailable {
        job: job_id.clone(),
        unavailable,
        span: span.clone(),
    })
}

fn first_unavailable_job_condition_reference(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) => None,
        Expr::NamedValue(name) => {
            if ["github", "needs", "vars", "inputs"]
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
            {
                None
            } else {
                Some(format!("the `{name}` context"))
            }
        }
        Expr::Not(inner) | Expr::Wildcard { target: inner } => {
            first_unavailable_job_condition_reference(inner)
        }
        Expr::Index { target, index }
        | Expr::Binary {
            lhs: target,
            rhs: index,
            ..
        } => first_unavailable_job_condition_reference(target)
            .or_else(|| first_unavailable_job_condition_reference(index)),
        Expr::Call { name, args } => {
            if name.eq_ignore_ascii_case("hashfiles") {
                return Some("the `hashFiles` function".to_string());
            }
            args.iter()
                .find_map(first_unavailable_job_condition_reference)
        }
    }
}

fn resolve_job_runner(
    job: &Job,
    job_id: &JobId,
    ctx: &FoldCtx<'_>,
) -> Result<RunnerImage, PlanError> {
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

pub(crate) fn strip_wrapper(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
        .map(str::trim)
        .unwrap_or(trimmed)
}

pub(crate) fn eval_err(job: &JobId, step: Option<&str>, source: PartialEvalError) -> PlanError {
    PlanError::Eval {
        job: job.clone(),
        step: step.map(str::to_string),
        source: Box::new(source),
    }
}

/// Design memo's implicit-`success()`-gate rule, computed from the
/// *authored* condition text (before folding) — see `JobPlan::implicit_status_gate`.
pub(crate) fn expr_calls_status(raw: &str) -> bool {
    match greenlit_expr::parse(strip_wrapper(raw)) {
        Ok(expr) => greenlit_expr::expr_calls_status_function(&expr),
        // A parse failure here would already have surfaced (as a real
        // error) from `fold_job_condition`'s own `plan_condition` call, so
        // this is only reached defensively; treating it as "no status
        // function" is the conservative (implicit-gate-applies) default.
        Err(_) => false,
    }
}

pub(crate) fn matrix_leg_value(leg: &MatrixLeg) -> Value {
    Value::object(
        leg.values
            .iter()
            .map(|(k, v)| (k.clone(), matrix_value_to_expr_value(v)))
            .collect(),
    )
}

fn matrix_value_to_expr_value(v: &MatrixValue) -> Value {
    match v {
        MatrixValue::Null => Value::Null,
        MatrixValue::Bool(b) => Value::Bool(*b),
        MatrixValue::Number(n) => Value::Number(*n),
        MatrixValue::String(s) => Value::String(s.clone()),
        MatrixValue::Sequence(items) => {
            Value::array(items.iter().map(matrix_value_to_expr_value).collect())
        }
        MatrixValue::Mapping(m) => Value::object(
            m.iter()
                .map(|(k, v)| (k.clone(), matrix_value_to_expr_value(v)))
                .collect(),
        ),
    }
}

/// Design memo §4.3, "Reference-to-producer check (error)": every
/// `needs.<job>.outputs.*`/`.result` reference found anywhere in this job
/// (its own condition, each leg's condition, every step's condition, and
/// every output value) must name a job in this job's own `needs:` list.
/// Also emits the design memo §4.3 "declared-output check (warning)":
/// referencing a real dependency's output it never declares is a lint, not
/// an error, since GitHub itself accepts it (yielding an empty string).
fn validate_needs_references(
    referencing: &ReferencingJob<'_>,
    parts: &JobPlanParts<'_>,
    workflow: &Workflow,
    lints: &mut Vec<Lint>,
) -> Result<(), PlanError> {
    let mut reasons = std::collections::BTreeSet::new();
    if let Some(c) = parts.condition {
        collect_condition_reasons(c, &mut reasons);
    }
    for leg in parts.legs {
        collect_step_reasons(&leg.steps, &mut reasons);
        collect_output_reasons(&leg.outputs, &mut reasons);
    }
    collect_step_reasons(parts.steps, &mut reasons);
    collect_output_reasons(parts.outputs, &mut reasons);

    for reason in &reasons {
        let referenced = match reason {
            DeferReason::NeedsOutput { job, .. } => Some(job),
            DeferReason::NeedsResult { job } => Some(job),
            _ => None,
        };
        let Some(referenced) = referenced else {
            continue;
        };
        if !referencing.needs.contains(referenced) {
            return Err(PlanError::NeedsReferenceNotDeclared {
                job: referencing.id.clone(),
                referenced: referenced.clone(),
            });
        }
        if let DeferReason::NeedsOutput {
            output: Some(name), ..
        } = reason
        {
            let declares_it = workflow
                .jobs
                .iter()
                .find(|j| j.id.value == referenced.0)
                .map(|j| j.outputs.iter().any(|(k, _)| &k.value == name))
                .unwrap_or(false);
            if !declares_it {
                lints.push(Lint::undeclared_needed_output(
                    referencing.span.clone(),
                    &referenced.0,
                    name,
                ));
            }
        }
    }
    Ok(())
}

/// The referencing job's identity, for [`validate_needs_references`].
struct ReferencingJob<'a> {
    id: &'a JobId,
    span: &'a greenlit_workflow::Span,
    needs: &'a [JobId],
}

/// The parts of a not-yet-assembled [`JobPlan`] [`validate_needs_references`]
/// needs to scan — bundled to keep the function's argument count small.
struct JobPlanParts<'a> {
    condition: &'a Option<Condition>,
    legs: &'a [LegPlan],
    steps: &'a [StepPlan],
    outputs: &'a JobOutputsPlan,
}

fn collect_condition_reasons(c: &Condition, out: &mut std::collections::BTreeSet<DeferReason>) {
    if let crate::condition::PlannedCond::Deferred(d) = &c.eval {
        out.extend(d.defers_on.iter().cloned());
    }
}

fn collect_step_reasons(steps: &[StepPlan], out: &mut std::collections::BTreeSet<DeferReason>) {
    for step in steps {
        if let Some(condition) = &step.condition {
            collect_condition_reasons(condition, out);
        }
    }
}

fn collect_output_reasons(
    outputs: &JobOutputsPlan,
    out: &mut std::collections::BTreeSet<DeferReason>,
) {
    for output in outputs.entries.values() {
        if let crate::outputs::PlannedValue::Deferred(deferred) = &output.value {
            out.extend(deferred.defers_on.iter().cloned());
        }
    }
}

/// Design memo §4.3, "Matrix-outputs collision (warning)": a matrix job's
/// output map is shared across all its legs (the last leg to finish always
/// wins — a well-known GHA limitation); warn when such a job's outputs are
/// actually read by a dependent, since that dependent's result is then
/// effectively nondeterministic across parallel legs.
pub(crate) fn lint_matrix_output_collisions(jobs: &[JobPlan], lints: &mut Vec<Lint>) {
    for producer in jobs {
        if producer.legs.len() <= 1
            || !producer
                .legs
                .iter()
                .any(|leg| !leg.outputs.entries.is_empty())
        {
            continue;
        }
        let referenced = jobs.iter().any(|dependent| {
            dependent.needs.contains(&producer.id)
                && references_needs_output_of(dependent, &producer.id)
        });
        if referenced {
            lints.push(Lint::matrix_outputs_collision(
                producer.span.clone(),
                &producer.id.0,
            ));
        }
    }
}

fn references_needs_output_of(job: &JobPlan, producer: &JobId) -> bool {
    let mut reasons = std::collections::BTreeSet::new();
    if let Some(c) = &job.condition {
        collect_condition_reasons(c, &mut reasons);
    }
    for leg in &job.legs {
        if let Some(c) = &leg.condition {
            collect_condition_reasons(c, &mut reasons);
        }
        collect_step_reasons(&leg.steps, &mut reasons);
        collect_output_reasons(&leg.outputs, &mut reasons);
    }
    collect_step_reasons(&job.steps, &mut reasons);
    collect_output_reasons(&job.outputs, &mut reasons);
    reasons
        .iter()
        .any(|r| matches!(r, DeferReason::NeedsOutput { job, .. } if job == producer))
}
