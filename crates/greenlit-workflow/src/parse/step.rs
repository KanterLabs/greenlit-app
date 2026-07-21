//! `jobs.<id>.steps[]:`.

use crate::error::ParseError;
use crate::model::step::{Step, StepAction};
use crate::parse::util::{
    as_mapping, as_sequence, find, raw_string, reject_unknown_keys, scalar_or_expr_map,
    spanned_scalar_or_expr,
};
use crate::span::Spanned;
use crate::yaml::raw::RawNode;

const STEP_KEYS: &[&str] = &[
    "id",
    "if",
    "name",
    "uses",
    "run",
    "shell",
    "with",
    "env",
    "working-directory",
    "continue-on-error",
    "timeout-minutes",
];

pub(crate) fn parse_steps(node: &Spanned<RawNode>, context: &str) -> Result<Vec<Step>, ParseError> {
    let items = as_sequence(node, context)?;
    items.iter().map(parse_step).collect()
}

fn parse_step(node: &Spanned<RawNode>) -> Result<Step, ParseError> {
    let entries = as_mapping(node, "step")?;
    reject_unknown_keys(entries, STEP_KEYS, "step")?;

    let id = find(entries, "id")
        .map(|v| raw_string(v, "step.id"))
        .transpose()?;
    let if_condition = find(entries, "if")
        .map(|v| raw_string(v, "step.if"))
        .transpose()?;
    let name = find(entries, "name")
        .map(|v| spanned_scalar_or_expr(v, "step.name"))
        .transpose()?;
    let env = match find(entries, "env") {
        Some(v) => scalar_or_expr_map(v, "step.env")?,
        None => Vec::new(),
    };
    let working_directory = find(entries, "working-directory")
        .map(|v| spanned_scalar_or_expr(v, "step.working-directory"))
        .transpose()?;
    let continue_on_error = find(entries, "continue-on-error")
        .map(|v| spanned_scalar_or_expr(v, "step.continue-on-error"))
        .transpose()?;
    let timeout_minutes = find(entries, "timeout-minutes")
        .map(|v| spanned_scalar_or_expr(v, "step.timeout-minutes"))
        .transpose()?;

    let run_node = find(entries, "run");
    let uses_node = find(entries, "uses");
    let action = match (run_node, uses_node) {
        (Some(run_node), None) => {
            let script = raw_string(run_node, "step.run")?;
            let shell = find(entries, "shell")
                .map(|v| raw_string(v, "step.shell"))
                .transpose()?;
            StepAction::Run { script, shell }
        }
        (None, Some(uses_node)) => {
            let reference = raw_string(uses_node, "step.uses")?;
            let with = match find(entries, "with") {
                Some(v) => scalar_or_expr_map(v, "step.with")?,
                None => Vec::new(),
            };
            StepAction::Uses { reference, with }
        }
        (Some(_), Some(_)) => {
            return Err(ParseError::Schema {
                span: node.span.clone(),
                message: "step must not have both 'run' and 'uses'".to_owned(),
            });
        }
        (None, None) => {
            return Err(ParseError::Schema {
                span: node.span.clone(),
                message: "step must have either 'run' or 'uses'".to_owned(),
            });
        }
    };
    if matches!(action, StepAction::Uses { .. }) && find(entries, "shell").is_some() {
        return Err(ParseError::Schema {
            span: node.span.clone(),
            message: "'shell' is only valid on a 'run' step".to_owned(),
        });
    }
    if matches!(action, StepAction::Run { .. }) && find(entries, "with").is_some() {
        return Err(ParseError::Schema {
            span: node.span.clone(),
            message: "'with' is only valid on a 'uses' step".to_owned(),
        });
    }

    Ok(Step {
        span: node.span.clone(),
        id,
        if_condition,
        name,
        env,
        working_directory,
        continue_on_error,
        timeout_minutes,
        action,
    })
}
