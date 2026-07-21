//! Assembles one [`StepPlan`].

use greenlit_expr::value::to_display_string;
use greenlit_workflow::model::job::Job;
use greenlit_workflow::model::step::{Step, StepAction};
use greenlit_workflow::model::value::ScalarOrExpr;
use greenlit_workflow::model::workflow::Workflow;

use crate::condition::plan_condition;
use crate::convert::scalar_to_value;
use crate::graph::JobId;
use crate::partial_eval::{
    FoldCtx, PartialEvalError, StaticRoots, TemplateFold, build_env_chain, fold_template,
};
use crate::pass_through::env_layer_to_map;

use super::job::{eval_err, expr_calls_status};
use super::{PlanError, StepKind, StepPlan};

pub(crate) fn plan_step(
    workflow: &Workflow,
    job: &Job,
    step: &Step,
    steps_roots: &StaticRoots<'_>,
    job_id: &JobId,
) -> Result<StepPlan, PlanError> {
    let step_id = step.id.as_ref().map(|s| s.value.clone());

    let step_env_layers: [&[(
        greenlit_workflow::Spanned<String>,
        greenlit_workflow::Spanned<ScalarOrExpr>,
    )]; 3] = [&workflow.env, &job.env, &step.env];
    let step_env_chain = build_env_chain(&step_env_layers, *steps_roots)
        .map_err(|source| eval_err(job_id, step_id.as_deref(), source))?;
    let step_ctx = FoldCtx {
        roots: *steps_roots,
        env: &step_env_chain,
        secrets_forbidden: false,
    };

    let condition = match &step.if_condition {
        None => None,
        Some(raw) => Some(
            plan_condition(&raw.value, &step_ctx)
                .map_err(|source| eval_err(job_id, step_id.as_deref(), source))?,
        ),
    };
    let implicit_status_gate = match &step.if_condition {
        None => true,
        Some(raw) => !expr_calls_status(&raw.value),
    };

    let name = match &step.name {
        None => None,
        Some(n) => Some(
            resolve_display_name(&n.value, &step_ctx)
                .map_err(|source| eval_err(job_id, step_id.as_deref(), source))?,
        ),
    };

    let kind = match &step.action {
        StepAction::Run { script, shell } => StepKind::Run {
            script: script.value.clone(),
            shell: shell.as_ref().map(|s| s.value.clone()),
        },
        StepAction::Uses { reference, with } => StepKind::Uses {
            reference: reference.value.clone(),
            with: env_layer_to_map(with),
        },
    };

    Ok(StepPlan {
        id: step_id,
        name,
        env: env_layer_to_map(&step.env),
        condition,
        implicit_status_gate,
        kind,
    })
}

fn resolve_display_name(v: &ScalarOrExpr, ctx: &FoldCtx<'_>) -> Result<String, PartialEvalError> {
    match v {
        ScalarOrExpr::Literal(s) => Ok(to_display_string(&scalar_to_value(s))),
        ScalarOrExpr::Expression(text) => match fold_template(text, ctx)? {
            TemplateFold::Static(v) => Ok(to_display_string(&v)),
            TemplateFold::Deferred { residual_text, .. } => Ok(residual_text),
        },
    }
}
