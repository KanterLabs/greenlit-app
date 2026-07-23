//! Assembles one [`StepPlan`] with every context-sensitive field explicitly
//! static or runtime-deferred.

use greenlit_workflow::model::step::{Step, StepAction};
use greenlit_workflow::model::value::ScalarOrExpr;

use crate::condition::plan_condition;
use crate::graph::JobId;
use crate::partial_eval::{EnvChain, FoldCtx, StaticRoots, extend_env_chain};
use crate::pass_through::plan_env_layer;
use crate::planned::{
    Planned, plan_scalar_bool, plan_scalar_number, plan_scalar_string, plan_template_string,
};

use super::conditions::expr_calls_status;
use super::error::{eval_err, located_eval_err, retained_field_err};
use super::{PlanError, PlanSizeBudget, StepKind, StepPlan};

pub(crate) fn plan_step(
    step: &Step,
    steps_roots: &StaticRoots<'_>,
    job_env: &EnvChain,
    job_id: &JobId,
    prior_step_can_mutate_env: bool,
    size_budget: &mut PlanSizeBudget,
) -> Result<StepPlan, PlanError> {
    let step_id = step.id.as_ref().map(|id| id.value.clone());
    if let (Some(id), Some(planned_id)) = (&step.id, &step_id) {
        size_budget.add(planned_id, &id.span)?;
    }
    let mut outer_env = job_env.clone();
    if prior_step_can_mutate_env {
        outer_env = outer_env.after_executable_step();
    }
    let step_layer_ctx = FoldCtx {
        roots: *steps_roots,
        env: &outer_env,
        secrets_forbidden: false,
    };
    let env = plan_env_layer(&step.env, &step_layer_ctx, size_budget)
        .map_err(|error| retained_field_err(job_id, step_id.as_deref(), error))?;

    let step_env = extend_env_chain(&outer_env, &step.env, *steps_roots, true)
        .map_err(|error| located_eval_err(job_id, step_id.as_deref(), error))?;
    let step_ctx = FoldCtx {
        roots: *steps_roots,
        env: &step_env,
        secrets_forbidden: false,
    };

    let condition = step
        .if_condition
        .as_ref()
        .map(|raw| {
            plan_condition(&raw.value, &raw.span, &step_ctx)
                .map_err(|source| eval_err(job_id, step_id.as_deref(), raw.span.clone(), source))
        })
        .transpose()?;
    if let (Some(raw), Some(planned)) = (&step.if_condition, &condition) {
        size_budget.add(planned, &raw.span)?;
    }
    let implicit_status_gate = step
        .if_condition
        .as_ref()
        .is_none_or(|raw| !expr_calls_status(&raw.value));

    let name = step
        .name
        .as_ref()
        .map(|name| {
            plan_scalar_string(name, &step_ctx)
                .map_err(|source| eval_err(job_id, step_id.as_deref(), name.span.clone(), source))
        })
        .transpose()?;
    if let (Some(raw), Some(planned)) = (&step.name, &name) {
        size_budget.add(planned, &raw.span)?;
    }
    let working_directory = plan_optional_scalar_string(
        step.working_directory.as_ref(),
        &step_ctx,
        job_id,
        step_id.as_deref(),
    )?;
    if let (Some(raw), Some(planned)) = (&step.working_directory, &working_directory) {
        size_budget.add(planned, &raw.span)?;
    }
    let continue_on_error = step
        .continue_on_error
        .as_ref()
        .map(|value| {
            plan_scalar_bool(value, &step_ctx)
                .map_err(|source| eval_err(job_id, step_id.as_deref(), value.span.clone(), source))
        })
        .transpose()?;
    if let (Some(raw), Some(planned)) = (&step.continue_on_error, &continue_on_error) {
        size_budget.add(planned, &raw.span)?;
    }
    let timeout_minutes = step
        .timeout_minutes
        .as_ref()
        .map(|value| {
            plan_scalar_number(value, &step_ctx)
                .map_err(|source| eval_err(job_id, step_id.as_deref(), value.span.clone(), source))
        })
        .transpose()?;
    if let (Some(raw), Some(planned)) = (&step.timeout_minutes, &timeout_minutes) {
        size_budget.add(planned, &raw.span)?;
    }

    let kind = match &step.action {
        StepAction::Run { script, shell } => {
            let script_plan = plan_template_string(&script.value, &script.span, &step_ctx)
                .map_err(|source| {
                    eval_err(job_id, step_id.as_deref(), script.span.clone(), source)
                })?;
            size_budget.add(&script_plan, &script.span)?;
            let shell_plan = shell.as_ref().map(|shell| {
                Planned::static_value(shell.span.clone(), shell.value.clone(), shell.value.clone())
            });
            if let (Some(raw), Some(planned)) = (shell, &shell_plan) {
                size_budget.add(planned, &raw.span)?;
            }
            StepKind::Run {
                script: Box::new(script_plan),
                shell: shell_plan,
            }
        }
        StepAction::Uses { reference, with } => {
            size_budget.add(&reference.value, &reference.span)?;
            StepKind::Uses {
                reference: reference.value.clone(),
                with: plan_env_layer(with, &step_ctx, size_budget)
                    .map_err(|error| retained_field_err(job_id, step_id.as_deref(), error))?,
            }
        }
    };

    size_budget.add_bytes(192, &step.span)?;

    Ok(StepPlan {
        id: step_id,
        name,
        env,
        working_directory,
        continue_on_error,
        timeout_minutes,
        condition,
        implicit_status_gate,
        kind,
    })
}

fn plan_optional_scalar_string(
    value: Option<&greenlit_workflow::Spanned<ScalarOrExpr>>,
    ctx: &FoldCtx<'_>,
    job_id: &JobId,
    step_id: Option<&str>,
) -> Result<Option<crate::planned::Planned<String>>, PlanError> {
    value
        .map(|value| {
            plan_scalar_string(value, ctx)
                .map_err(|source| eval_err(job_id, step_id, value.span.clone(), source))
        })
        .transpose()
}
