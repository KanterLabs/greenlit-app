//! The workflow root mapping.

use crate::error::ParseError;
use crate::model::value::UnsupportedConstruct;
use crate::model::workflow::{PermissionLevel, PermissionLevelAll, Permissions, Workflow};
use crate::parse::job::parse_jobs;
use crate::parse::trigger::parse_on;
use crate::parse::util::{
    as_mapping, find, find_pair, key_text, parse_defaults, raw_string, reject_unknown_keys,
    require, scalar_or_expr_map,
};
use crate::span::Spanned;
use crate::yaml::raw::{RawNode, parse_raw};
use std::path::Path;
use std::sync::Arc;

const WORKFLOW_KEYS: &[&str] = &[
    "name",
    "on",
    "env",
    "defaults",
    "permissions",
    "concurrency",
    "jobs",
];

// The `permissions` mapping is a closed schema. This list is transcribed
// from GitHub's current "Defining access for the GITHUB_TOKEN scopes"
// table, including `artifact-metadata`, `code-quality`, and
// `vulnerability-alerts`:
// https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#defining-access-for-the-github_token-scopes
const PERMISSION_SCOPES: &[&str] = &[
    "actions",
    "artifact-metadata",
    "attestations",
    "checks",
    "code-quality",
    "contents",
    "deployments",
    "discussions",
    "id-token",
    "issues",
    "models",
    "packages",
    "pages",
    "pull-requests",
    "security-events",
    "statuses",
    "vulnerability-alerts",
];

/// Parse a workflow document's source text into the typed model.
///
/// `file_name` is stored on every node's [`crate::span::Span`] verbatim
/// (a repo-relative path is conventional, e.g. `.github/workflows/ci.yml`,
/// but this function does no path handling itself — see
/// [`parse_workflow_file`] for a convenience that reads from disk).
///
/// # Errors
/// Returns [`ParseError`] for any YAML syntax problem or workflow schema
/// violation. See the crate-level docs for the unknown-key policy and known
/// limitations.
pub fn parse_workflow(
    file_name: impl Into<Arc<str>>,
    source: &str,
) -> Result<Workflow, ParseError> {
    let file = file_name.into();
    let root = parse_raw(file, source)?;
    let entries = as_mapping(&root, "workflow")?;
    reject_unknown_keys(entries, WORKFLOW_KEYS, "workflow")?;

    let name = find(entries, "name")
        .map(|v| raw_string(v, "workflow.name"))
        .transpose()?;
    let on_node = require(entries, "on", &root.span, "workflow")?;
    let on = parse_on(on_node)?;
    let env = match find(entries, "env") {
        Some(v) => scalar_or_expr_map(v, "workflow.env")?,
        None => Vec::new(),
    };
    let defaults = find(entries, "defaults")
        .map(|v| {
            Ok::<_, ParseError>(Spanned::new(
                parse_defaults(v, "workflow.defaults")?,
                v.span.clone(),
            ))
        })
        .transpose()?;
    let permissions = find(entries, "permissions")
        .map(|v| Ok::<_, ParseError>(Spanned::new(parse_permissions(v)?, v.span.clone())))
        .transpose()?;
    let concurrency = find_pair(entries, "concurrency").map(|(k, _)| UnsupportedConstruct {
        name: "concurrency",
        location: k.span.clone(),
    });
    let jobs_node = require(entries, "jobs", &root.span, "workflow")?;
    let jobs = parse_jobs(jobs_node)?;

    Ok(Workflow {
        span: root.span.clone(),
        name,
        on,
        env,
        defaults,
        permissions,
        concurrency,
        jobs,
    })
}

/// [`parse_workflow`], reading `path` from disk first and using it
/// (lossily converted to UTF-8 if needed) as the stored file name.
///
/// # Errors
/// Returns [`ParseError::Io`] if `path` cannot be read, or any error
/// [`parse_workflow`] can return.
pub fn parse_workflow_file(path: impl AsRef<Path>) -> Result<Workflow, ParseError> {
    let path = path.as_ref();
    let file: Arc<str> = Arc::from(path.to_string_lossy().into_owned());
    let source = std::fs::read_to_string(path).map_err(|e| ParseError::Io {
        path: file.clone(),
        message: e.to_string(),
    })?;
    parse_workflow(file, &source)
}

fn parse_permissions(node: &Spanned<RawNode>) -> Result<Permissions, ParseError> {
    match &node.value {
        RawNode::Scalar(_) => {
            let text = raw_string(node, "permissions")?;
            match text.value.as_str() {
                "read-all" => Ok(Permissions::All(PermissionLevelAll::ReadAll)),
                "write-all" => Ok(Permissions::All(PermissionLevelAll::WriteAll)),
                other => Err(ParseError::Schema {
                    span: text.span,
                    message: format!("permissions '{other}' must be 'read-all' or 'write-all'"),
                }),
            }
        }
        RawNode::Mapping(_) => {
            let entries = as_mapping(node, "permissions")?;
            reject_unknown_keys(entries, PERMISSION_SCOPES, "permissions")?;
            let mut scopes = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                let scope = Spanned::new(key_text(k)?.to_owned(), k.span.clone());
                let level_text = raw_string(v, "permissions.<scope>")?;
                let level = match level_text.value.as_str() {
                    "read" => PermissionLevel::Read,
                    "write" => PermissionLevel::Write,
                    "none" => PermissionLevel::None,
                    other => {
                        return Err(ParseError::Schema {
                            span: level_text.span,
                            message: format!(
                                "permission level '{other}' must be one of read|write|none"
                            ),
                        });
                    }
                };
                scopes.push((scope, Spanned::new(level, level_text.span)));
            }
            Ok(Permissions::Scoped(scopes))
        }
        RawNode::Sequence(_) => Err(ParseError::Schema {
            span: node.span.clone(),
            message: "permissions must be a string or a mapping".to_owned(),
        }),
    }
}
