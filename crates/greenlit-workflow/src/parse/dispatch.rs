//! `workflow_dispatch` input declarations.

use crate::error::ParseError;
use crate::model::trigger::{WorkflowDispatch, WorkflowDispatchInput, WorkflowDispatchInputType};
use crate::model::value::{ScalarOrExpr, YamlScalar, YamlValue};
use crate::parse::util::{
    as_mapping, expect_bool, expect_string, expect_string_sequence, find, key_text,
    reject_unknown_keys, to_yaml_value,
};
use crate::span::Spanned;
use crate::yaml::raw::RawNode;

const DISPATCH_INPUT_KEYS: &[&str] = &["description", "required", "default", "type", "options"];

pub(super) fn parse_workflow_dispatch(
    node: &Spanned<RawNode>,
) -> Result<WorkflowDispatch, ParseError> {
    let entries = as_mapping(node, "on.workflow_dispatch")?;
    reject_unknown_keys(entries, &["inputs"], "on.workflow_dispatch")?;
    let inputs = match find(entries, "inputs") {
        Some(value) => parse_inputs(value)?,
        None => Vec::new(),
    };
    Ok(WorkflowDispatch { inputs })
}

fn parse_inputs(node: &Spanned<RawNode>) -> Result<Vec<WorkflowDispatchInput>, ParseError> {
    let entries = as_mapping(node, "on.workflow_dispatch.inputs")?;
    // Current workflow syntax allows at most 25 top-level inputs:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onworkflow_dispatchinputs
    if entries.len() > 25 {
        return Err(ParseError::Schema {
            span: node.span.clone(),
            message: format!(
                "on.workflow_dispatch.inputs defines {} inputs, exceeding the maximum of 25",
                entries.len()
            ),
        });
    }
    entries
        .iter()
        .map(|(key, value)| parse_dispatch_input(key, value))
        .collect()
}

fn parse_dispatch_input(
    key: &Spanned<RawNode>,
    value: &Spanned<RawNode>,
) -> Result<WorkflowDispatchInput, ParseError> {
    let name = Spanned::new(key_text(key)?.to_owned(), key.span.clone());
    validate_input_name(&name)?;
    let entries = as_mapping(value, "on.workflow_dispatch.inputs.<name>")?;
    reject_unknown_keys(
        entries,
        DISPATCH_INPUT_KEYS,
        "on.workflow_dispatch.inputs.<name>",
    )?;
    let description = find(entries, "description")
        .map(|node| expect_string(node, "on.workflow_dispatch.inputs.<name>.description"))
        .transpose()?;
    let required = find(entries, "required")
        .map(|node| expect_bool(node, "on.workflow_dispatch.inputs.<name>.required"))
        .transpose()?;
    let default =
        find(entries, "default").map(|node| Spanned::new(to_yaml_value(node), node.span.clone()));
    let input_type = find(entries, "type").map(parse_input_type).transpose()?;
    let options = match find(entries, "options") {
        Some(node) => expect_string_sequence(node, "on.workflow_dispatch.inputs.<name>.options")?,
        None => Vec::new(),
    };
    validate_input_shape(value, input_type.as_ref(), default.as_ref(), &options)?;
    Ok(WorkflowDispatchInput {
        span: value.span.clone(),
        name,
        description,
        required,
        default,
        input_type,
        options,
    })
}

fn validate_input_name(name: &Spanned<String>) -> Result<(), ParseError> {
    // Manual-dispatch inputs were introduced with the same declaration
    // format as action inputs, whose identifier grammar is documented here:
    // https://github.blog/changelog/2020-07-06-github-actions-manual-triggers-with-workflow_dispatch/
    // https://docs.github.com/en/actions/reference/workflows-and-actions/metadata-syntax#inputsinput_id
    let mut chars = name.value.chars();
    let starts_validly = chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
    let rest_is_valid = chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if starts_validly && rest_is_valid {
        Ok(())
    } else {
        Err(ParseError::Schema {
            span: name.span.clone(),
            message: format!(
                "workflow_dispatch input '{}' must start with a letter or '_' and contain only letters, digits, '-' or '_'",
                name.value
            ),
        })
    }
}

fn validate_input_shape(
    declaration: &Spanned<RawNode>,
    input_type: Option<&Spanned<WorkflowDispatchInputType>>,
    default: Option<&Spanned<YamlValue>>,
    options: &[Spanned<String>],
) -> Result<(), ParseError> {
    // `choice` is the one selectable-option type; the other four input
    // types do not have an `options` declaration:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onworkflow_dispatchinputsinput_idtype
    let kind = input_type
        .map(|input_type| input_type.value)
        .unwrap_or(WorkflowDispatchInputType::String);
    if kind == WorkflowDispatchInputType::Choice {
        if options.is_empty() {
            return Err(ParseError::Schema {
                span: declaration.span.clone(),
                message: "workflow_dispatch choice input must declare at least one option"
                    .to_owned(),
            });
        }
    } else if let Some(option) = options.first() {
        return Err(ParseError::Schema {
            span: option.span.clone(),
            message: "workflow_dispatch input options are only valid for type 'choice'".to_owned(),
        });
    }
    validate_default(kind, default)
}

fn validate_default(
    kind: WorkflowDispatchInputType,
    default: Option<&Spanned<YamlValue>>,
) -> Result<(), ParseError> {
    // Dispatch values retain their declared Boolean/number/string kinds in
    // the `inputs` context, so a default must have that same declared kind:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onworkflow_dispatchinputs
    let Some(default) = default else {
        return Ok(());
    };
    let expected = match kind {
        WorkflowDispatchInputType::Boolean => "a boolean",
        WorkflowDispatchInputType::Number => "a number",
        WorkflowDispatchInputType::String
        | WorkflowDispatchInputType::Choice
        | WorkflowDispatchInputType::Environment => "a string",
    };
    let type_matches = matches!(
        (kind, &default.value),
        (
            WorkflowDispatchInputType::Boolean,
            YamlValue::Scalar(ScalarOrExpr::Literal(YamlScalar::Bool(_)))
        ) | (
            WorkflowDispatchInputType::Number,
            YamlValue::Scalar(ScalarOrExpr::Literal(YamlScalar::Number(_)))
        ) | (
            WorkflowDispatchInputType::String
                | WorkflowDispatchInputType::Choice
                | WorkflowDispatchInputType::Environment,
            YamlValue::Scalar(ScalarOrExpr::Literal(YamlScalar::String(_)))
        )
    );
    if type_matches {
        Ok(())
    } else {
        Err(ParseError::Schema {
            span: default.span.clone(),
            message: format!("workflow_dispatch {kind:?} input default must be {expected}"),
        })
    }
}

fn parse_input_type(
    node: &Spanned<RawNode>,
) -> Result<Spanned<WorkflowDispatchInputType>, ParseError> {
    let text = expect_string(node, "on.workflow_dispatch.inputs.<name>.type")?;
    let kind = match text.value.as_str() {
        "string" => WorkflowDispatchInputType::String,
        "boolean" => WorkflowDispatchInputType::Boolean,
        "number" => WorkflowDispatchInputType::Number,
        "choice" => WorkflowDispatchInputType::Choice,
        "environment" => WorkflowDispatchInputType::Environment,
        other => {
            return Err(ParseError::Schema {
                span: text.span,
                message: format!(
                    "input type '{other}' is not one of string|boolean|number|choice|environment"
                ),
            });
        }
    };
    Ok(Spanned::new(kind, text.span))
}
