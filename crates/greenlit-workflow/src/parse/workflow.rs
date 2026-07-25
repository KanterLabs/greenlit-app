//! The workflow root mapping.

use crate::error::ParseError;
use crate::model::workflow::{
    Concurrency, PermissionLevel, PermissionLevelAll, Permissions, Workflow,
};
use crate::parse::job::parse_jobs;
use crate::parse::trigger::parse_on;
use crate::parse::util::{
    as_mapping, find, key_text, parse_defaults, raw_string, reject_unknown_keys, require,
    scalar_or_expr_map, spanned_scalar_or_expr,
};
use crate::span::Spanned;
use crate::yaml::raw::{RawNode, parse_raw};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

/// GitHub's default maximum workflow-source size, measured as UTF-16 code
/// units (the semantics of .NET `String.Length`).
///
/// Pinned runner source:
/// <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/ParseOptions.cs>
pub const MAX_WORKFLOW_SOURCE_CHARACTERS: usize = 1024 * 1024;

// A valid UTF-8 string uses at most three bytes per UTF-16 code unit. Reading
// one extra byte lets the disk boundary reject an arbitrarily large file
// before allocating it, while still admitting every source at the exact
// character limit for the authoritative UTF-16 count below.
const MAX_BOUNDED_SOURCE_BYTES: usize = MAX_WORKFLOW_SOURCE_CHARACTERS * 3;

const WORKFLOW_KEYS: &[&str] = &[
    "name",
    "run-name",
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

fn permission_level(scope: &str, level: Spanned<String>) -> Result<PermissionLevel, ParseError> {
    // GitHub's workflow syntax publishes a scope-specific value union:
    // `id-token` is write-only, while `models` and
    // `vulnerability-alerts` are read-only. Every other current scope
    // accepts read, write, or none.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#defining-access-for-the-github_token-scopes
    let parsed = match level.value.as_str() {
        "read" if scope != "id-token" => Some(PermissionLevel::Read),
        "write" if !matches!(scope, "models" | "vulnerability-alerts") => {
            Some(PermissionLevel::Write)
        }
        "none" => Some(PermissionLevel::None),
        _ => None,
    };
    parsed.ok_or_else(|| {
        let allowed = match scope {
            "id-token" => "write|none",
            "models" | "vulnerability-alerts" => "read|none",
            _ => "read|write|none",
        };
        ParseError::Schema {
            span: level.span,
            message: format!(
                "permission '{scope}' level '{}' must be one of {allowed}",
                level.value
            ),
        }
    })
}

/// Parse a workflow document's source text into the typed model.
///
/// `file_name` is stored on every node's [`crate::span::Span`] verbatim
/// (a repo-relative path is conventional, e.g. `.github/workflows/ci.yml`,
/// but this function does no path handling itself — see
/// [`parse_workflow_file`] for a convenience that reads from disk).
///
/// # Errors
/// Returns [`ParseError`] for any YAML syntax problem or workflow schema
/// violation. See the crate-level docs for the unknown-key policy and
/// deliberate v0 scope decisions.
pub fn parse_workflow(
    file_name: impl Into<Arc<str>>,
    source: &str,
) -> Result<Workflow, ParseError> {
    let file = file_name.into();
    if source.encode_utf16().count() > MAX_WORKFLOW_SOURCE_CHARACTERS {
        return Err(ParseError::SourceLimit {
            path: file,
            max_characters: MAX_WORKFLOW_SOURCE_CHARACTERS,
        });
    }
    let root = parse_raw(file, source)?;
    let entries = as_mapping(&root, "workflow")?;
    reject_unknown_keys(entries, WORKFLOW_KEYS, "workflow")?;

    let name = find(entries, "name")
        .map(|v| raw_string(v, "workflow.name"))
        .transpose()?;
    let run_name = find(entries, "run-name")
        .map(|v| raw_string(v, "workflow.run-name"))
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
        .map(|v| {
            Ok::<_, ParseError>(Spanned::new(
                parse_permissions(v, "permissions")?,
                v.span.clone(),
            ))
        })
        .transpose()?;
    let concurrency = find(entries, "concurrency")
        .map(parse_concurrency)
        .transpose()?;
    let jobs_node = require(entries, "jobs", &root.span, "workflow")?;
    let jobs = parse_jobs(jobs_node)?;

    let workflow = Workflow {
        span: root.span.clone(),
        name,
        run_name,
        on,
        env,
        defaults,
        permissions,
        concurrency,
        jobs,
    };
    crate::validate::validate_workflow(&workflow)?;
    Ok(workflow)
}

pub(crate) fn parse_concurrency(
    node: &Spanned<RawNode>,
) -> Result<Spanned<Concurrency>, ParseError> {
    let concurrency = match &node.value {
        RawNode::Scalar(_) => Concurrency {
            group: raw_string(node, "concurrency")?,
            cancel_in_progress: None,
        },
        RawNode::Mapping(entries) => {
            reject_unknown_keys(entries, &["group", "cancel-in-progress"], "concurrency")?;
            let group = raw_string(
                require(entries, "group", &node.span, "concurrency")?,
                "concurrency.group",
            )?;
            let cancel_in_progress = find(entries, "cancel-in-progress")
                .map(|value| spanned_scalar_or_expr(value, "concurrency.cancel-in-progress"))
                .transpose()?;
            Concurrency {
                group,
                cancel_in_progress,
            }
        }
        RawNode::Sequence(_) => {
            return Err(ParseError::Schema {
                span: node.span.clone(),
                message: "concurrency must be a string or mapping".to_string(),
            });
        }
    };
    Ok(Spanned::new(concurrency, node.span.clone()))
}

/// [`parse_workflow`], reading `path` from disk first and using it
/// (lossily converted to UTF-8 if needed) as the stored file name.
///
/// # Errors
/// Returns [`ParseError::Io`] if `path` cannot be read,
/// [`ParseError::Encoding`] if its contents are not UTF-8, or any error
/// [`parse_workflow`] can return.
pub fn parse_workflow_file(path: impl AsRef<Path>) -> Result<Workflow, ParseError> {
    let path = path.as_ref();
    let file: Arc<str> = Arc::from(path.to_string_lossy().into_owned());
    parse_workflow_file_with_name(path, file)
}

/// [`parse_workflow_file`], while storing the caller-provided repository-
/// relative `file_name` in source spans and diagnostics.
///
/// The file read is capped before allocation at the largest possible UTF-8
/// representation of GitHub's source-character limit. This is the entry
/// point for callers that already resolved a filesystem path but need stable
/// repository-relative source identities.
///
/// # Errors
/// Returns [`ParseError::Io`] if `path` cannot be read,
/// [`ParseError::Encoding`] if its contents are not UTF-8,
/// [`ParseError::SourceLimit`] when it exceeds GitHub's limit, or any error
/// [`parse_workflow`] can return.
pub fn parse_workflow_file_with_name(
    path: impl AsRef<Path>,
    file_name: impl Into<Arc<str>>,
) -> Result<Workflow, ParseError> {
    let path = path.as_ref();
    let file = file_name.into();
    let opened = std::fs::File::open(path).map_err(|e| ParseError::Io {
        path: file.clone(),
        message: e.to_string(),
    })?;
    let byte_limit = u64::try_from(MAX_BOUNDED_SOURCE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    opened
        .take(byte_limit)
        .read_to_end(&mut bytes)
        .map_err(|e| ParseError::Io {
            path: file.clone(),
            message: e.to_string(),
        })?;
    if bytes.len() > MAX_BOUNDED_SOURCE_BYTES {
        return Err(ParseError::SourceLimit {
            path: file,
            max_characters: MAX_WORKFLOW_SOURCE_CHARACTERS,
        });
    }
    let source = String::from_utf8(bytes).map_err(|error| ParseError::Encoding {
        path: file.clone(),
        message: error.to_string(),
    })?;
    parse_workflow(file, &source)
}

pub(crate) fn parse_permissions(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Permissions, ParseError> {
    match &node.value {
        RawNode::Scalar(_) => {
            let text = raw_string(node, context)?;
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
            let entries = as_mapping(node, context)?;
            reject_unknown_keys(entries, PERMISSION_SCOPES, context)?;
            let mut scopes = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                let scope = Spanned::new(key_text(k)?.to_owned(), k.span.clone());
                let level_text = raw_string(v, &format!("{context}.<scope>"))?;
                let level_span = level_text.span.clone();
                let level = permission_level(&scope.value, level_text)?;
                scopes.push((scope, Spanned::new(level, level_span)));
            }
            Ok(Permissions::Scoped(scopes))
        }
        RawNode::Sequence(_) => Err(ParseError::Schema {
            span: node.span.clone(),
            message: "permissions must be a string or a mapping".to_owned(),
        }),
    }
}
