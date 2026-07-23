//! `jobs.<id>:`.

use crate::error::ParseError;
use crate::model::job::{ContainerSpec, Job, RunsOn};
use crate::model::value::UnsupportedConstruct;
use crate::parse::container::parse_container;
use crate::parse::matrix::parse_strategy;
use crate::parse::step::parse_steps;
use crate::parse::util::{
    Entries, as_mapping, find, find_pair, key_text, parse_defaults, raw_string,
    reject_unknown_keys, require, scalar_or_expr_map, string_list,
};
use crate::parse::workflow::parse_permissions;
use crate::span::Spanned;
use crate::yaml::raw::RawNode;

/// One declared `jobs.<id>.outputs.<name>` entry: its name paired with the
/// raw expression text (retained, not evaluated — see [`Job::outputs`]).
type JobOutputs = Vec<(Spanned<String>, Spanned<String>)>;

/// One `jobs.<id>.services:` entry: its name paired with its container
/// spec (shared with `container:` — see [`crate::parse::container`]).
type ServiceList = Vec<(Spanned<String>, Spanned<ContainerSpec>)>;

const REUSABLE_JOB_KEYS: &[&str] = &[
    "name",
    "needs",
    "if",
    "uses",
    "with",
    "secrets",
    "strategy",
    "concurrency",
    "permissions",
];
const JOB_KEYS: &[&str] = &[
    "name",
    "runs-on",
    "needs",
    "if",
    "outputs",
    "env",
    "defaults",
    "permissions",
    "strategy",
    "services",
    "container",
    "steps",
    "environment",
    "concurrency",
];

/// Parse `jobs:`, requiring at least one entry.
pub(crate) fn parse_jobs(node: &Spanned<RawNode>) -> Result<Vec<Job>, ParseError> {
    let entries = as_mapping(node, "jobs")?;
    if entries.is_empty() {
        return Err(ParseError::Schema {
            span: node.span.clone(),
            message: "workflow must define at least one job".to_owned(),
        });
    }
    entries.iter().map(|(k, v)| parse_job(k, v)).collect()
}

fn parse_job(key: &Spanned<RawNode>, value: &Spanned<RawNode>) -> Result<Job, ParseError> {
    let id = Spanned::new(key_text(key)?.to_owned(), key.span.clone());
    let entries = as_mapping(value, "job")?;
    let context = format!("job '{}'", id.value);

    // A job defined with `uses:` is a reusable-workflow call, not a
    // steps-running job — recognized but rejected (reusable workflows are
    // out of v0 scope, `greenlit-v0-spec.md` "Out (v0)").
    if let Some((uses_key, _)) = find_pair(entries, "uses") {
        reject_unknown_keys(entries, REUSABLE_JOB_KEYS, &context)?;
        let name = find(entries, "name")
            .map(|v| raw_string(v, &context))
            .transpose()?;
        let needs = parse_needs(entries)?;
        let if_condition = find(entries, "if")
            .map(|v| raw_string(v, &context))
            .transpose()?;
        let permissions = parse_job_permissions(entries, &context)?;
        return Ok(Job {
            span: value.span.clone(),
            id,
            name,
            runs_on: None,
            needs,
            if_condition,
            outputs: Vec::new(),
            env: Vec::new(),
            defaults: None,
            permissions,
            strategy: None,
            services: Vec::new(),
            container: None,
            steps: Vec::new(),
            environment: None,
            concurrency: None,
            reusable_call: Some(UnsupportedConstruct {
                name: "reusable workflow call (jobs.<id>.uses)",
                location: uses_key.span.clone(),
            }),
        });
    }

    reject_unknown_keys(entries, JOB_KEYS, &context)?;
    let name = find(entries, "name")
        .map(|v| raw_string(v, &context))
        .transpose()?;
    let runs_on_node = require(entries, "runs-on", &value.span, &context)?;
    let runs_on = Some(Spanned::new(
        parse_runs_on(runs_on_node)?,
        runs_on_node.span.clone(),
    ));
    let needs = parse_needs(entries)?;
    let if_condition = find(entries, "if")
        .map(|v| raw_string(v, &context))
        .transpose()?;
    let outputs = parse_outputs(entries, &context)?;
    let env = match find(entries, "env") {
        Some(v) => scalar_or_expr_map(v, &context)?,
        None => Vec::new(),
    };
    let defaults = find(entries, "defaults")
        .map(|v| Ok::<_, ParseError>(Spanned::new(parse_defaults(v, &context)?, v.span.clone())))
        .transpose()?;
    let permissions = parse_job_permissions(entries, &context)?;
    let strategy = find(entries, "strategy")
        .map(|v| Ok::<_, ParseError>(Spanned::new(parse_strategy(v)?, v.span.clone())))
        .transpose()?;
    let services = parse_services(entries, &context)?;
    let container = find(entries, "container")
        .map(|v| Ok::<_, ParseError>(Spanned::new(parse_container(v, &context)?, v.span.clone())))
        .transpose()?;
    let steps_node = require(entries, "steps", &value.span, &context)?;
    let steps = parse_steps(steps_node, &context)?;
    if steps.is_empty() {
        return Err(ParseError::Schema {
            span: steps_node.span.clone(),
            message: "job must have at least one step".to_owned(),
        });
    }
    let environment = find_pair(entries, "environment").map(|(k, _)| UnsupportedConstruct {
        name: "environment",
        location: k.span.clone(),
    });
    let concurrency = find_pair(entries, "concurrency").map(|(k, _)| UnsupportedConstruct {
        name: "concurrency",
        location: k.span.clone(),
    });

    Ok(Job {
        span: value.span.clone(),
        id,
        name,
        runs_on,
        needs,
        if_condition,
        outputs,
        env,
        defaults,
        permissions,
        strategy,
        services,
        container,
        steps,
        environment,
        concurrency,
        reusable_call: None,
    })
}

fn parse_job_permissions(
    entries: &Entries,
    context: &str,
) -> Result<Option<Spanned<crate::model::workflow::Permissions>>, ParseError> {
    find(entries, "permissions")
        .map(|node| {
            Ok(Spanned::new(
                parse_permissions(node, &format!("{context}.permissions"))?,
                node.span.clone(),
            ))
        })
        .transpose()
}

fn parse_needs(entries: &Entries) -> Result<Vec<Spanned<String>>, ParseError> {
    match find(entries, "needs") {
        Some(v) => string_list(v, "needs"),
        None => Ok(Vec::new()),
    }
}

fn parse_outputs(entries: &Entries, context: &str) -> Result<JobOutputs, ParseError> {
    match find(entries, "outputs") {
        Some(v) => {
            let out_entries = as_mapping(v, context)?;
            out_entries
                .iter()
                .map(|(k, v)| {
                    let name = Spanned::new(key_text(k)?.to_owned(), k.span.clone());
                    Ok((name, raw_string(v, context)?))
                })
                .collect()
        }
        None => Ok(Vec::new()),
    }
}

fn parse_services(entries: &Entries, context: &str) -> Result<ServiceList, ParseError> {
    match find(entries, "services") {
        Some(v) => {
            let svc_entries = as_mapping(v, context)?;
            svc_entries
                .iter()
                .map(|(k, v)| {
                    let name = Spanned::new(key_text(k)?.to_owned(), k.span.clone());
                    let spec = Spanned::new(parse_container(v, context)?, v.span.clone());
                    Ok((name, spec))
                })
                .collect()
        }
        None => Ok(Vec::new()),
    }
}

fn parse_runs_on(node: &Spanned<RawNode>) -> Result<RunsOn, ParseError> {
    match &node.value {
        RawNode::Scalar(_) => Ok(RunsOn::Label(raw_string(node, "runs-on")?)),
        RawNode::Sequence(_) => Ok(RunsOn::Labels(string_list(node, "runs-on")?)),
        RawNode::Mapping(_) => {
            let entries = as_mapping(node, "runs-on")?;
            reject_unknown_keys(entries, &["group", "labels"], "runs-on")?;
            let group = find(entries, "group")
                .map(|v| raw_string(v, "runs-on.group"))
                .transpose()?;
            let labels = match find(entries, "labels") {
                Some(v) => string_list(v, "runs-on.labels")?,
                None => Vec::new(),
            };
            Ok(RunsOn::Group { group, labels })
        }
    }
}
