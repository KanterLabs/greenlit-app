//! `workflow_dispatch` input resolution and synthetic payload assembly.

use std::collections::HashMap;

use greenlit_expr::Value;
use greenlit_workflow::model::trigger::{
    Trigger, WorkflowDispatchInput, WorkflowDispatchInputType,
};
use greenlit_workflow::model::workflow::Workflow;

use crate::convert::yaml_value_to_value;

use super::payload::branch_ref_fields;
use super::{EventError, EventPayload};

pub(super) fn workflow_dispatch_payload(
    workflow: &Workflow,
    provided: &HashMap<String, String>,
    branch: &str,
) -> Result<EventPayload, EventError> {
    let declared_inputs: &[WorkflowDispatchInput] = workflow
        .on
        .iter()
        .find_map(|trigger| match &trigger.value {
            Trigger::WorkflowDispatch(dispatch) => Some(dispatch.inputs.as_slice()),
            _ => None,
        })
        .unwrap_or(&[]);

    let declared_names: std::collections::BTreeSet<&str> = declared_inputs
        .iter()
        .map(|input| input.name.value.as_str())
        .collect();
    if let Some(unknown) = provided
        .keys()
        .filter(|name| !declared_names.contains(name.as_str()))
        .min()
    {
        return Err(EventError::UnknownInput {
            name: unknown.clone(),
            declared: declared_names
                .iter()
                .copied()
                .collect::<Vec<_>>()
                .join(", "),
        });
    }

    let mut event_inputs = Vec::with_capacity(declared_inputs.len());
    let mut typed_inputs = Vec::with_capacity(declared_inputs.len());
    for input in declared_inputs {
        let raw = provided
            .get(&input.name.value)
            .cloned()
            .or_else(|| {
                input
                    .default
                    .as_ref()
                    .map(|default| yaml_value_to_display(&default.value))
            })
            .or_else(|| {
                if input
                    .required
                    .as_ref()
                    .is_some_and(|required| required.value)
                {
                    None
                } else {
                    Some(match input.input_type.as_ref().map(|kind| kind.value) {
                        Some(WorkflowDispatchInputType::Boolean) => "false".to_string(),
                        Some(WorkflowDispatchInputType::Number) => "0".to_string(),
                        _ => String::new(),
                    })
                }
            })
            .ok_or_else(|| EventError::MissingRequiredInput {
                span: input.span.clone(),
                name: input.name.value.clone(),
            })?;
        event_inputs.push((input.name.value.clone(), Value::String(raw.clone())));
        typed_inputs.push((input.name.value.clone(), typed_input_value(input, &raw)?));
    }

    // The workflow_dispatch webhook payload documents `ref` as required;
    // the Actions event table says GITHUB_REF is the branch or tag that
    // received the dispatch. This synthetic event dispatches the checked
    // out local branch.
    // https://docs.github.com/en/webhooks/webhook-events-and-payloads#workflow_dispatch
    // https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_dispatch
    let payload = Value::object(vec![
        ("inputs".to_string(), Value::object(event_inputs)),
        ("ref".to_string(), Value::String(branch.to_string())),
    ]);
    Ok((
        payload,
        branch_ref_fields(branch),
        Value::object(typed_inputs),
    ))
}

fn yaml_value_to_display(value: &greenlit_workflow::model::value::YamlValue) -> String {
    greenlit_expr::value::to_display_string(&yaml_value_to_value(value))
}

/// `github.event.inputs.*` is always a string; the top-level `inputs`
/// context reflects the declared type. Boolean/number remain typed, while
/// choice/environment/string values remain strings.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#inputs-context
fn typed_input_value(input: &WorkflowDispatchInput, raw: &str) -> Result<Value, EventError> {
    match input.input_type.as_ref().map(|kind| kind.value) {
        Some(WorkflowDispatchInputType::Boolean) => {
            if raw.eq_ignore_ascii_case("true") {
                Ok(Value::Bool(true))
            } else if raw.eq_ignore_ascii_case("false") {
                Ok(Value::Bool(false))
            } else {
                Err(invalid_input(input, raw, "`true` or `false`"))
            }
        }
        Some(WorkflowDispatchInputType::Number) => raw
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .map(Value::Number)
            .ok_or_else(|| invalid_input(input, raw, "a finite number")),
        Some(WorkflowDispatchInputType::Choice)
            if !input.options.iter().any(|option| option.value == raw) =>
        {
            Err(invalid_input(
                input,
                raw,
                &format!(
                    "one of: {}",
                    input
                        .options
                        .iter()
                        .map(|option| option.value.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ))
        }
        _ => Ok(Value::String(raw.to_string())),
    }
}

fn invalid_input(input: &WorkflowDispatchInput, value: &str, expected: &str) -> EventError {
    EventError::InvalidInputValue {
        span: input.span.clone(),
        name: input.name.value.clone(),
        expected: expected.to_string(),
        value: value.to_string(),
    }
}
