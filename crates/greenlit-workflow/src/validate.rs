//! Public-model expression validation performed before a parsed workflow is
//! returned. Keeping the traversal here makes the context policy auditable
//! field by field and ensures static extraction never acts as validation.

use crate::error::ParseError;
use crate::expression::{ExpressionPolicy, reject_template, validate_if, validate_template};
use crate::model::job::{ContainerSpec, Job, Matrix, MatrixSource, RunsOn, Strategy};
use crate::model::step::{Step, StepAction};
use crate::model::value::{ScalarOrExpr, YamlValue};
use crate::model::workflow::{Defaults, Workflow};
use crate::span::Spanned;
use std::collections::HashMap;

pub(crate) fn validate_workflow(workflow: &Workflow) -> Result<(), ParseError> {
    if let Some(name) = &workflow.name {
        reject_template(&name.value, &name.span, "name")?;
    }
    for (_, value) in &workflow.env {
        validate_scalar(value, "env", ExpressionPolicy::WorkflowEnv)?;
    }
    if let Some(defaults) = &workflow.defaults {
        validate_defaults(&defaults.value, "defaults.run", true)?;
    }
    for job in &workflow.jobs {
        validate_job(job)?;
    }
    Ok(())
}

fn validate_job(job: &Job) -> Result<(), ParseError> {
    validate_identifier(&job.id, "job id")?;
    let prefix = format!("jobs.{}", job.id.value);
    if let Some(name) = &job.name {
        validate_template(
            &name.value,
            &name.span,
            &format!("{prefix}.name"),
            ExpressionPolicy::JobStrategyContext,
        )?;
    }
    if let Some(runs_on) = &job.runs_on {
        validate_runs_on(&runs_on.value, &prefix)?;
    }
    for need in &job.needs {
        reject_template(&need.value, &need.span, &format!("{prefix}.needs"))?;
    }
    if let Some(condition) = &job.if_condition {
        validate_if(
            &condition.value,
            &condition.span,
            &format!("{prefix}.if"),
            ExpressionPolicy::JobIf,
        )?;
    }
    for (_, output) in &job.outputs {
        validate_template(
            &output.value,
            &output.span,
            &format!("{prefix}.outputs"),
            ExpressionPolicy::JobOutputs,
        )?;
    }
    for (_, value) in &job.env {
        validate_scalar(value, &format!("{prefix}.env"), ExpressionPolicy::JobEnv)?;
    }
    if let Some(defaults) = &job.defaults {
        validate_defaults(&defaults.value, &format!("{prefix}.defaults.run"), false)?;
    }
    if let Some(strategy) = &job.strategy {
        validate_strategy(&strategy.value, &prefix)?;
    }
    for (service_name, service) in &job.services {
        validate_container(
            &service.value,
            &format!("{prefix}.services.{}", service_name.value),
        )?;
    }
    if let Some(container) = &job.container {
        validate_container(&container.value, &format!("{prefix}.container"))?;
    }
    // The workflow syntax requires every authored step id to be unique. The
    // runner stores the `steps` context in `DictionaryContextData`, whose key
    // index uses `StringComparer.OrdinalIgnoreCase`; two ids differing only in
    // ASCII case therefore address the same step-context slot:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepsid
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTPipelines/Pipelines/ContextData/DictionaryContextData.cs#L71-L84
    let mut step_ids = HashMap::new();
    for (index, step) in job.steps.iter().enumerate() {
        let context = format!("{prefix}.steps[{index}]");
        if let Some(id) = &step.id {
            validate_step_id(id, &context)?;
            let folded = id.value.to_ascii_lowercase();
            if let Some(first_span) = step_ids.get(&folded) {
                return Err(ParseError::Schema {
                    span: id.span.clone(),
                    message: format!(
                        "step id '{}' is not unique within job '{}' because step ids are case-insensitive; rename it (first declared at {first_span})",
                        id.value, job.id.value
                    ),
                });
            }
            step_ids.insert(folded, id.span.clone());
        }
        validate_step(step, &context)?;
    }
    Ok(())
}

fn validate_step_id(id: &Spanned<String>, context: &str) -> Result<(), ParseError> {
    reject_template(&id.value, &id.span, &format!("{context}.id"))?;
    validate_identifier(id, "step id")
}

fn validate_identifier(id: &Spanned<String>, kind: &str) -> Result<(), ParseError> {
    // GitHub documents this grammar for `jobs.<job_id>` and for context
    // property names, which is how an authored step id is referenced:
    // https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-jobs#setting-an-id-for-a-job
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#about-contexts
    let mut bytes = id.value.bytes();
    let valid = bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(ParseError::Schema {
            span: id.span.clone(),
            message: format!(
                "{kind} '{}' must start with a letter or '_' and contain only ASCII letters, digits, '-' or '_'",
                id.value
            ),
        })
    }
}

fn validate_runs_on(runs_on: &RunsOn, prefix: &str) -> Result<(), ParseError> {
    let context = format!("{prefix}.runs-on");
    let labels: Vec<_> = match runs_on {
        RunsOn::Label(label) => vec![label],
        RunsOn::Labels(labels) => labels.iter().collect(),
        RunsOn::Group { group, labels } => group.iter().chain(labels).collect(),
    };
    for label in labels {
        validate_template(
            &label.value,
            &label.span,
            &context,
            ExpressionPolicy::JobStrategyContext,
        )?;
    }
    Ok(())
}

fn validate_defaults(
    defaults: &Defaults,
    context: &str,
    workflow_level: bool,
) -> Result<(), ParseError> {
    let Some(run) = &defaults.run else {
        return Ok(());
    };
    if let Some(shell) = &run.value.shell {
        if workflow_level {
            reject_template(&shell.value, &shell.span, context)?;
        } else {
            validate_template(
                &shell.value,
                &shell.span,
                context,
                ExpressionPolicy::JobDefaultsRun,
            )?;
        }
    }
    if let Some(working_directory) = &run.value.working_directory {
        if workflow_level {
            if let ScalarOrExpr::Expression(text) = &working_directory.value {
                reject_template(text, &working_directory.span, context)?;
            }
        } else {
            validate_scalar(working_directory, context, ExpressionPolicy::JobDefaultsRun)?;
        }
    }
    Ok(())
}

fn validate_strategy(strategy: &Strategy, prefix: &str) -> Result<(), ParseError> {
    let context = format!("{prefix}.strategy");
    if let Some(matrix) = &strategy.matrix {
        match &matrix.value {
            MatrixSource::Expression(expression) => validate_template(
                &expression.value,
                &expression.span,
                &format!("{context}.matrix"),
                ExpressionPolicy::JobStrategy,
            )?,
            MatrixSource::Inline(matrix) => validate_matrix(matrix, &context)?,
        }
    }
    if let Some(fail_fast) = &strategy.fail_fast {
        validate_scalar(fail_fast, &context, ExpressionPolicy::JobStrategy)?;
    }
    if let Some(max_parallel) = &strategy.max_parallel {
        validate_scalar(max_parallel, &context, ExpressionPolicy::JobStrategy)?;
    }
    Ok(())
}

fn validate_matrix(matrix: &Matrix, context: &str) -> Result<(), ParseError> {
    for (_, values) in &matrix.axes {
        for value in values {
            validate_yaml(value, context, ExpressionPolicy::JobStrategy)?;
        }
    }
    for entry in matrix.include.iter().chain(&matrix.exclude) {
        for (_, value) in &entry.value {
            validate_yaml(value, context, ExpressionPolicy::JobStrategy)?;
        }
    }
    Ok(())
}

fn validate_container(container: &ContainerSpec, context: &str) -> Result<(), ParseError> {
    validate_scalar(
        &container.image,
        &format!("{context}.image"),
        ExpressionPolicy::Container,
    )?;
    if let Some(credentials) = &container.credentials {
        if let Some(username) = &credentials.value.username {
            validate_scalar(
                username,
                &format!("{context}.credentials"),
                ExpressionPolicy::ContainerCredentials,
            )?;
        }
        if let Some(password) = &credentials.value.password {
            validate_scalar(
                password,
                &format!("{context}.credentials"),
                ExpressionPolicy::ContainerCredentials,
            )?;
        }
    }
    for (_, value) in &container.env {
        validate_scalar(
            value,
            &format!("{context}.env"),
            ExpressionPolicy::ContainerEnv,
        )?;
    }
    for value in container.ports.iter().chain(&container.volumes) {
        validate_scalar(value, context, ExpressionPolicy::Container)?;
    }
    if let Some(options) = &container.options {
        validate_scalar(options, context, ExpressionPolicy::Container)?;
    }
    Ok(())
}

fn validate_step(step: &Step, context: &str) -> Result<(), ParseError> {
    if let Some(condition) = &step.if_condition {
        validate_if(
            &condition.value,
            &condition.span,
            &format!("{context}.if"),
            ExpressionPolicy::StepIf,
        )?;
    }
    if let Some(name) = &step.name {
        validate_scalar(name, &format!("{context}.name"), ExpressionPolicy::Step)?;
    }
    for (_, value) in &step.env {
        validate_scalar(value, &format!("{context}.env"), ExpressionPolicy::Step)?;
    }
    for value in [
        step.working_directory.as_ref(),
        step.continue_on_error.as_ref(),
        step.timeout_minutes.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_scalar(value, context, ExpressionPolicy::Step)?;
    }
    match &step.action {
        StepAction::Run { script, shell } => {
            validate_template(
                &script.value,
                &script.span,
                &format!("{context}.run"),
                ExpressionPolicy::Step,
            )?;
            if let Some(shell) = shell {
                validate_template(
                    &shell.value,
                    &shell.span,
                    &format!("{context}.shell"),
                    ExpressionPolicy::Step,
                )?;
            }
        }
        StepAction::Uses { reference, with } => {
            reject_template(
                &reference.value,
                &reference.span,
                &format!("{context}.uses"),
            )?;
            for (_, value) in with {
                validate_scalar(value, &format!("{context}.with"), ExpressionPolicy::Step)?;
            }
        }
    }
    Ok(())
}

fn validate_scalar(
    value: &Spanned<ScalarOrExpr>,
    context: &str,
    policy: ExpressionPolicy,
) -> Result<(), ParseError> {
    if let ScalarOrExpr::Expression(text) = &value.value {
        validate_template(text, &value.span, context, policy)?;
    }
    Ok(())
}

fn validate_yaml(
    value: &Spanned<YamlValue>,
    context: &str,
    policy: ExpressionPolicy,
) -> Result<(), ParseError> {
    match &value.value {
        YamlValue::Scalar(ScalarOrExpr::Expression(text)) => {
            validate_template(text, &value.span, context, policy)
        }
        YamlValue::Scalar(ScalarOrExpr::Literal(_)) => Ok(()),
        YamlValue::Sequence(items) => {
            for item in items {
                validate_yaml(item, context, policy)?;
            }
            Ok(())
        }
        YamlValue::Mapping(entries) => {
            for (_, nested) in entries {
                validate_yaml(nested, context, policy)?;
            }
            Ok(())
        }
    }
}
