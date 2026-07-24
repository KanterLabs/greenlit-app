//! Parses `runs:`, dispatching on `using:` into the node/composite/docker
//! shapes.

use indexmap::IndexMap;

use crate::manifest::yaml::RawNode;
use crate::manifest::{CompositeStep, DockerRuns, ManifestError, NodeRuns, NodeVersion, Runs};
use greenlit_workflow::{Span, Spanned};

use super::util::{
    as_mapping, as_sequence, find, optional_string, reject_unknown_keys, require, required_string,
};

const NODE_KEYS: &[&str] = &["using", "main", "pre", "pre-if", "post", "post-if"];
const COMPOSITE_KEYS: &[&str] = &["using", "steps"];
const DOCKER_KEYS: &[&str] = &["using", "image", "args", "entrypoint", "env"];
const STEP_KEYS: &[&str] = &[
    "id",
    "name",
    "if",
    "run",
    "shell",
    "working-directory",
    "env",
    "uses",
    "with",
    "continue-on-error",
];

pub(super) fn parse_runs(node: &Spanned<RawNode>) -> Result<Runs, ManifestError> {
    let entries = as_mapping(node, "runs")?;
    let using_node = require(entries, "using", &node.span, "runs")?;
    let using_span = using_node.span.clone();
    let using = super::util::scalar_string(using_node, "runs.using")?.unwrap_or_default();

    match using.as_str() {
        "node20" => parse_node(entries, NodeVersion::Node20, &using_span),
        "node24" => parse_node(entries, NodeVersion::Node24, &using_span),
        "composite" => parse_composite(entries, &using_span),
        "docker" => parse_docker(entries, &using_span),
        _ => Err(ManifestError::UnsupportedUsing {
            span: using_span,
            value: using,
        }),
    }
}

fn parse_node(
    entries: &super::util::Entries,
    using: NodeVersion,
    using_span: &Span,
) -> Result<Runs, ManifestError> {
    reject_unknown_keys(entries, NODE_KEYS, "runs")?;
    Ok(Runs::Node(NodeRuns {
        using,
        main: required_string(entries, "main", using_span, "runs")?,
        pre: optional_string(entries, "pre", "runs")?,
        pre_if: optional_string(entries, "pre-if", "runs")?,
        post: optional_string(entries, "post", "runs")?,
        post_if: optional_string(entries, "post-if", "runs")?,
    }))
}

fn parse_composite(
    entries: &super::util::Entries,
    using_span: &Span,
) -> Result<Runs, ManifestError> {
    reject_unknown_keys(entries, COMPOSITE_KEYS, "runs")?;
    let steps_node = require(entries, "steps", using_span, "runs")?;
    let items = as_sequence(steps_node, "runs.steps")?;
    let steps = items
        .iter()
        .enumerate()
        .map(|(index, item)| parse_step(item, index))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Runs::Composite { steps })
}

fn parse_docker(entries: &super::util::Entries, using_span: &Span) -> Result<Runs, ManifestError> {
    reject_unknown_keys(entries, DOCKER_KEYS, "runs")?;
    let image = required_string(entries, "image", using_span, "runs")?;
    let args = match find(entries, "args") {
        Some(node) => as_sequence(node, "runs.args")?
            .iter()
            .map(|item| {
                super::util::scalar_string(item, "runs.args[]")?.ok_or_else(|| {
                    ManifestError::Schema {
                        span: item.span.clone(),
                        message: "runs.args[] must not be an explicit null".to_owned(),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    let env = match find(entries, "env") {
        Some(node) => parse_string_map(node, "runs.env")?,
        None => IndexMap::new(),
    };
    Ok(Runs::Docker(DockerRuns {
        image,
        args,
        entrypoint: optional_string(entries, "entrypoint", "runs")?,
        env,
    }))
}

fn parse_step(node: &Spanned<RawNode>, index: usize) -> Result<CompositeStep, ManifestError> {
    let context = format!("runs.steps[{index}]");
    let entries = as_mapping(node, &context)?;
    reject_unknown_keys(entries, STEP_KEYS, &context)?;

    let run = optional_string(entries, "run", &context)?;
    let uses = optional_string(entries, "uses", &context)?;
    match (&run, &uses) {
        (None, None) => {
            return Err(ManifestError::CompositeStepMissingRunOrUses {
                span: node.span.clone(),
            });
        }
        (Some(_), Some(_)) => {
            return Err(ManifestError::CompositeStepHasBothRunAndUses {
                span: node.span.clone(),
            });
        }
        _ => {}
    }
    let shell = optional_string(entries, "shell", &context)?;
    if run.is_some() && shell.is_none() {
        return Err(ManifestError::CompositeStepRunMissingShell {
            span: node.span.clone(),
        });
    }

    let env = match find(entries, "env") {
        Some(node) => parse_string_map(node, &format!("{context}.env"))?,
        None => IndexMap::new(),
    };
    let with = match find(entries, "with") {
        Some(node) => parse_string_map(node, &format!("{context}.with"))?,
        None => IndexMap::new(),
    };

    Ok(CompositeStep {
        id: optional_string(entries, "id", &context)?,
        name: optional_string(entries, "name", &context)?,
        if_condition: optional_string(entries, "if", &context)?,
        run,
        shell,
        working_directory: optional_string(entries, "working-directory", &context)?,
        env,
        uses,
        with,
        continue_on_error: optional_string(entries, "continue-on-error", &context)?,
    })
}

/// Parses a mapping of scalar name/value pairs (`env:`, `with:`) into an
/// order-preserving map of raw (possibly `${{ }}`-templated) string values.
fn parse_string_map(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<IndexMap<String, String>, ManifestError> {
    let entries = as_mapping(node, context)?;
    let mut map = IndexMap::with_capacity(entries.len());
    for (key, value) in entries {
        let Some(name) = key.value.as_key_text() else {
            return Err(ManifestError::NonScalarKey {
                span: key.span.clone(),
            });
        };
        let text =
            super::util::scalar_string(value, &format!("{context}.{name}"))?.unwrap_or_default();
        map.insert(name.to_owned(), text);
    }
    Ok(map)
}
