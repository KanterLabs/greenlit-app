//! `on:` — normalizing all three trigger-list YAML forms.

use crate::error::ParseError;
use crate::model::trigger::{
    Trigger, WebhookFilter, WorkflowDispatch, WorkflowDispatchInput, WorkflowDispatchInputType,
};
use crate::model::value::{UnsupportedConstruct, YamlScalar};
use crate::parse::util::{
    Entries, as_mapping, as_sequence, find, find_pair, key_text, raw_string, reject_unknown_keys,
    require, string_list, to_yaml_value,
};
use crate::span::Spanned;
use crate::yaml::raw::RawNode;

/// Normalize `on:` — a bare scalar, a sequence of scalars, or a mapping —
/// into a uniform list of triggers.
pub(crate) fn parse_on(node: &Spanned<RawNode>) -> Result<Vec<Spanned<Trigger>>, ParseError> {
    match &node.value {
        RawNode::Scalar(_) => Ok(vec![trigger_from_bare_name(node)?]),
        RawNode::Sequence(items) => items.iter().map(trigger_from_bare_name).collect(),
        RawNode::Mapping(entries) => entries
            .iter()
            .map(|(k, v)| trigger_from_entry(k, v))
            .collect(),
    }
}

fn trigger_from_bare_name(node: &Spanned<RawNode>) -> Result<Spanned<Trigger>, ParseError> {
    let name = raw_string(node, "on")?;
    build_trigger(name, None)
}

fn trigger_from_entry(
    key: &Spanned<RawNode>,
    value: &Spanned<RawNode>,
) -> Result<Spanned<Trigger>, ParseError> {
    let name = Spanned::new(key_text(key)?.to_owned(), key.span.clone());
    let config = match &value.value {
        RawNode::Scalar(s) if s.value == YamlScalar::Null => None,
        _ => Some(value),
    };
    build_trigger(name, config)
}

fn build_trigger(
    name: Spanned<String>,
    config: Option<&Spanned<RawNode>>,
) -> Result<Spanned<Trigger>, ParseError> {
    // A configured mapping/sequence is the trigger node's own value, so
    // its complete span—not the nearby event-name key—is the outer trigger
    // span. Bare/null forms have no configuration node and retain the event
    // name's span.
    let span = config.map_or_else(|| name.span.clone(), |node| node.span.clone());
    let trigger = match name.value.as_str() {
        // push/pull_request/pull_request_target share the same
        // branch/tag/path/type filter shape (design memo §5.2).
        "push" | "pull_request" | "pull_request_target" => Trigger::Webhook {
            name: name.value.clone(),
            filter: match config {
                Some(c) => parse_webhook_filter(c, &name.value)?,
                None => empty_webhook_filter(span.clone()),
            },
        },
        "workflow_dispatch" => Trigger::WorkflowDispatch(match config {
            Some(c) => parse_workflow_dispatch(c)?,
            None => WorkflowDispatch::default(),
        }),
        "schedule" => Trigger::Schedule(match config {
            Some(c) => parse_schedule(c)?,
            None => Vec::new(),
        }),
        "repository_dispatch" => Trigger::RepositoryDispatch {
            types: match config {
                Some(c) => {
                    let entries = as_mapping(c, "on.repository_dispatch")?;
                    reject_unknown_keys(entries, &["types"], "on.repository_dispatch")?;
                    match find(entries, "types") {
                        Some(v) => string_list(v, "on.repository_dispatch.types")?,
                        None => Vec::new(),
                    }
                }
                None => Vec::new(),
            },
        },
        // Reusable workflows are out of v0 scope (`greenlit-v0-spec.md`
        // "Out (v0)"); recognized so a file that triggers on it still
        // parses, rejected at planning time by `greenlit-engine`.
        "workflow_call" => Trigger::WorkflowCall(UnsupportedConstruct {
            name: "workflow_call",
            location: span.clone(),
        }),
        other => Trigger::Other {
            name: other.to_owned(),
            config: config.map(|c| Spanned::new(to_yaml_value(c), c.span.clone())),
        },
    };
    Ok(Spanned::new(trigger, span))
}

const WEBHOOK_FILTER_KEYS: &[&str] = &[
    "branches",
    "branches-ignore",
    "tags",
    "tags-ignore",
    "paths",
    "paths-ignore",
    "types",
];

fn parse_webhook_filter(
    node: &Spanned<RawNode>,
    event_name: &str,
) -> Result<WebhookFilter, ParseError> {
    let context = format!("on.{event_name}");
    let entries = as_mapping(node, &context)?;
    reject_unknown_keys(entries, WEBHOOK_FILTER_KEYS, &context)?;
    reject_conflicting_filters(entries, &context)?;
    let list = |key: &str| -> Result<Vec<Spanned<String>>, ParseError> {
        match find(entries, key) {
            Some(v) => string_list(v, &context),
            None => Ok(Vec::new()),
        }
    };
    Ok(WebhookFilter {
        span: node.span.clone(),
        branches: list("branches")?,
        branches_ignore: list("branches-ignore")?,
        tags: list("tags")?,
        tags_ignore: list("tags-ignore")?,
        paths: list("paths")?,
        paths_ignore: list("paths-ignore")?,
        types: list("types")?,
    })
}

fn empty_webhook_filter(span: crate::span::Span) -> WebhookFilter {
    WebhookFilter {
        span,
        branches: Vec::new(),
        branches_ignore: Vec::new(),
        tags: Vec::new(),
        tags_ignore: Vec::new(),
        paths: Vec::new(),
        paths_ignore: Vec::new(),
        types: Vec::new(),
    }
}

fn reject_conflicting_filters(entries: &Entries, context: &str) -> Result<(), ParseError> {
    // GitHub forbids each positive/ignore pair on one event and directs
    // authors to use `!` patterns in the positive filter instead:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushbranchestagsbranches-ignoretags-ignore
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushpull_requestpull_request_targetpathspaths-ignore
    for (positive, ignored) in [
        ("branches", "branches-ignore"),
        ("tags", "tags-ignore"),
        ("paths", "paths-ignore"),
    ] {
        if let (Some((positive_key, _)), Some((ignored_key, _))) =
            (find_pair(entries, positive), find_pair(entries, ignored))
        {
            return Err(ParseError::Schema {
                span: ignored_key.span.clone(),
                message: format!(
                    "{context} filter '{positive}' at {} conflicts with '{ignored}' at {}",
                    positive_key.span, ignored_key.span
                ),
            });
        }
    }
    Ok(())
}

fn parse_workflow_dispatch(node: &Spanned<RawNode>) -> Result<WorkflowDispatch, ParseError> {
    let entries = as_mapping(node, "on.workflow_dispatch")?;
    reject_unknown_keys(entries, &["inputs"], "on.workflow_dispatch")?;
    let inputs = match find(entries, "inputs") {
        Some(v) => {
            let input_entries = as_mapping(v, "on.workflow_dispatch.inputs")?;
            input_entries
                .iter()
                .map(|(k, v)| parse_dispatch_input(k, v))
                .collect::<Result<_, _>>()?
        }
        None => Vec::new(),
    };
    Ok(WorkflowDispatch { inputs })
}

const DISPATCH_INPUT_KEYS: &[&str] = &["description", "required", "default", "type", "options"];

fn parse_dispatch_input(
    key: &Spanned<RawNode>,
    value: &Spanned<RawNode>,
) -> Result<WorkflowDispatchInput, ParseError> {
    let name = Spanned::new(key_text(key)?.to_owned(), key.span.clone());
    let entries = as_mapping(value, "on.workflow_dispatch.inputs.<name>")?;
    reject_unknown_keys(
        entries,
        DISPATCH_INPUT_KEYS,
        "on.workflow_dispatch.inputs.<name>",
    )?;
    let description = find(entries, "description")
        .map(|v| raw_string(v, "on.workflow_dispatch.inputs.<name>.description"))
        .transpose()?;
    let required = find(entries, "required")
        .map(|v| crate::parse::util::expect_bool(v, "on.workflow_dispatch.inputs.<name>.required"))
        .transpose()?;
    let default = find(entries, "default").map(|v| Spanned::new(to_yaml_value(v), v.span.clone()));
    let input_type = find(entries, "type").map(parse_input_type).transpose()?;
    let options = match find(entries, "options") {
        Some(v) => string_list(v, "on.workflow_dispatch.inputs.<name>.options")?,
        None => Vec::new(),
    };
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

fn parse_input_type(
    node: &Spanned<RawNode>,
) -> Result<Spanned<WorkflowDispatchInputType>, ParseError> {
    let text = raw_string(node, "on.workflow_dispatch.inputs.<name>.type")?;
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

fn parse_schedule(node: &Spanned<RawNode>) -> Result<Vec<Spanned<String>>, ParseError> {
    let items = as_sequence(node, "on.schedule")?;
    items
        .iter()
        .map(|item| {
            let entries = as_mapping(item, "on.schedule[]")?;
            reject_unknown_keys(entries, &["cron"], "on.schedule[]")?;
            let cron = require(entries, "cron", &item.span, "on.schedule[]")?;
            raw_string(cron, "on.schedule[].cron")
        })
        .collect()
}
