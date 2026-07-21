//! `on:` — normalizing all three trigger-list YAML forms.

use crate::error::ParseError;
use crate::model::trigger::{Schedule, Trigger, WebhookFilter, WorkflowDispatch};
use crate::model::value::{UnsupportedConstruct, YamlScalar};
use crate::parse::util::{
    Entries, as_mapping, as_sequence, expect_string, find, find_pair, key_text, raw_string,
    reject_unknown_keys, require, string_list, to_yaml_value,
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
        // These events share the normalized filter model, while the parser
        // applies each event's distinct allowed-key schema below.
        "push" | "pull_request" | "pull_request_target" => Trigger::Webhook {
            name: name.value.clone(),
            filter: match config {
                Some(c) => parse_webhook_filter(c, &name.value)?,
                None => empty_webhook_filter(span.clone()),
            },
        },
        "workflow_dispatch" => Trigger::WorkflowDispatch(match config {
            Some(c) => super::dispatch::parse_workflow_dispatch(c)?,
            None => WorkflowDispatch::default(),
        }),
        "schedule" => Trigger::Schedule(match config {
            Some(c) => parse_schedule(c)?,
            None => {
                return Err(ParseError::Schema {
                    span: name.span.clone(),
                    message: "on.schedule must declare at least one cron mapping".to_owned(),
                });
            }
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

const PUSH_FILTER_KEYS: &[&str] = &[
    "branches",
    "branches-ignore",
    "tags",
    "tags-ignore",
    "paths",
    "paths-ignore",
];
// GitHub gives tag filters only to `push`, and activity `types` only to the
// pull-request events:
// https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushbranchestagsbranches-ignoretags-ignore
// https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpull_requestpull_request_targettypes
const PULL_REQUEST_FILTER_KEYS: &[&str] = &[
    "branches",
    "branches-ignore",
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
    let known = if event_name == "push" {
        PUSH_FILTER_KEYS
    } else {
        PULL_REQUEST_FILTER_KEYS
    };
    reject_unknown_keys(entries, known, &context)?;
    reject_conflicting_filters(entries, &context)?;
    let list = |key: &str| -> Result<Vec<Spanned<String>>, ParseError> {
        match find(entries, key) {
            Some(v) => {
                let patterns = string_list(v, &format!("{context}.{key}"))?;
                if patterns.is_empty() {
                    return Err(ParseError::Schema {
                        span: v.span.clone(),
                        message: format!("{context}.{key} must contain at least one pattern"),
                    });
                }
                if let Some(empty) = patterns.iter().find(|pattern| pattern.value.is_empty()) {
                    return Err(ParseError::Schema {
                        span: empty.span.clone(),
                        message: format!("{context}.{key} patterns must not be empty"),
                    });
                }
                Ok(patterns)
            }
            None => Ok(Vec::new()),
        }
    };
    let filter = WebhookFilter {
        span: node.span.clone(),
        branches: list("branches")?,
        branches_ignore: list("branches-ignore")?,
        tags: list("tags")?,
        tags_ignore: list("tags-ignore")?,
        paths: list("paths")?,
        paths_ignore: list("paths-ignore")?,
        types: list("types")?,
    };
    validate_negative_patterns(&filter.branches, "branches", &context)?;
    validate_negative_patterns(&filter.tags, "tags", &context)?;
    validate_negative_patterns(&filter.paths, "paths", &context)?;
    Ok(filter)
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

fn validate_negative_patterns(
    patterns: &[Spanned<String>],
    key: &str,
    context: &str,
) -> Result<(), ParseError> {
    // A positive filter may mix `!` exclusions with inclusions, but GitHub
    // requires at least one positive pattern so the set has something to
    // subtract from:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushbranchestagsbranches-ignoretags-ignore
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushpull_requestpull_request_targetpathspaths-ignore
    let first_negative = patterns
        .iter()
        .find(|pattern| pattern.value.starts_with('!'));
    let has_positive = patterns
        .iter()
        .any(|pattern| !pattern.value.starts_with('!'));
    if let Some(negative) = first_negative
        && !has_positive
    {
        return Err(ParseError::Schema {
            span: negative.span.clone(),
            message: format!(
                "{context}.{key} uses a negative pattern but has no positive pattern; use '{key}-ignore' for exclusions only"
            ),
        });
    }
    Ok(())
}

fn parse_schedule(node: &Spanned<RawNode>) -> Result<Vec<Schedule>, ParseError> {
    // A schedule is a nonempty sequence of mappings containing `cron` and,
    // on current GitHub.com, an optional IANA `timezone` string:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onschedule
    let items = as_sequence(node, "on.schedule")?;
    if items.is_empty() {
        return Err(ParseError::Schema {
            span: node.span.clone(),
            message: "on.schedule must declare at least one cron mapping".to_owned(),
        });
    }
    items
        .iter()
        .map(|item| {
            let entries = as_mapping(item, "on.schedule[]")?;
            reject_unknown_keys(entries, &["cron", "timezone"], "on.schedule[]")?;
            let cron = require(entries, "cron", &item.span, "on.schedule[]")?;
            let cron = expect_string(cron, "on.schedule[].cron")?;
            if cron.value.trim().is_empty() {
                return Err(ParseError::Schema {
                    span: cron.span,
                    message: "on.schedule[].cron must not be empty".to_owned(),
                });
            }
            let timezone = find(entries, "timezone")
                .map(|value| expect_string(value, "on.schedule[].timezone"))
                .transpose()?;
            if let Some(timezone) = &timezone
                && timezone.value.trim().is_empty()
            {
                return Err(ParseError::Schema {
                    span: timezone.span.clone(),
                    message: "on.schedule[].timezone must not be empty".to_owned(),
                });
            }
            Ok(Schedule {
                span: item.span.clone(),
                cron,
                timezone,
            })
        })
        .collect()
}
