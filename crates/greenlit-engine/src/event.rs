//! The synthetic trigger event builder: populates the `github` context (and,
//! for `workflow_dispatch`, the `inputs` context) without a real GitHub
//! webhook payload.
//!
//! Source: `PHASE-1-engine-core.md` greenlit-engine section — "Synthetic
//! event builder: default `push` event populated from local git metadata
//! (repo name, branch, SHA, actor from git config); `pull_request` and
//! `workflow_dispatch` variants with sensible synthetic payloads." Field
//! names below are GitHub's own documented `github` context properties
//! (Contexts reference, "github context"); this crate populates the subset
//! meaningful from local git and the selected synthetic event. Properties
//! assigned by GitHub or its runner remain absent here and are represented
//! as runtime-deferred by `crate::partial_eval`.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use greenlit_expr::Value;
use greenlit_workflow::model::trigger::Trigger;
use greenlit_workflow::model::workflow::Workflow;

use crate::git::{GitError, collect_git_context, file_matches_head};

mod dispatch;
mod filter;
mod payload;

use dispatch::workflow_dispatch_payload;
use filter::webhook_filter_matches;
use payload::{branch_ref_fields, pull_request_payload, push_event_payload};

pub(super) type EventPayload = (Value, Vec<(String, Value)>, Value);
pub(super) const SYNTHETIC_PULL_REQUEST_NUMBER: u64 = 1;

/// Which trigger `litci plan`/`litci run` is simulating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// `push` — the default when no `-e` is given.
    Push,
    /// `pull_request`.
    PullRequest,
    /// `workflow_dispatch`.
    WorkflowDispatch,
}

impl EventKind {
    /// The `github.event_name` value.
    #[must_use]
    pub fn event_name(&self) -> &'static str {
        match self {
            EventKind::Push => "push",
            EventKind::PullRequest => "pull_request",
            EventKind::WorkflowDispatch => "workflow_dispatch",
        }
    }
}

/// A fully-built synthetic trigger event: the `github` context object, and
/// (for `workflow_dispatch`) the typed `inputs` context object.
#[derive(Debug, Clone)]
pub struct SyntheticEvent {
    /// Which event this is.
    pub kind: EventKind,
    /// The `github` context, ready for [`greenlit_expr::Context::with_github`].
    pub github: Value,
    /// The `inputs` context, ready for [`greenlit_expr::Context::with_inputs`]
    /// — an empty object for `push`/`pull_request`.
    pub inputs: Value,
    /// Event-specific `github.*` properties whose documented value cannot
    /// be derived truthfully until runtime has materialized the event.
    ///
    /// For a synthetic pull request, GitHub defines `github.sha` as the PR
    /// merge commit rather than the local topic `HEAD`; `workflow_sha` is
    /// likewise the commit that supplied the workflow file. Phase 1 keeps
    /// both residual instead of inventing a plausible SHA.
    pub deferred_github_properties: BTreeSet<String>,
}

/// Everything that can go wrong building a synthetic event.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum EventError {
    /// Local git metadata could not be collected.
    #[error(transparent)]
    Git(#[from] GitError),
    /// `--input` was used with a non-dispatch event.
    #[error(
        "dispatch inputs were supplied for `{event}`; inputs are only valid for `workflow_dispatch`"
    )]
    InputsForNonDispatch {
        /// Selected event name.
        event: &'static str,
    },
    /// A CLI input name was not declared by the workflow.
    #[error("workflow_dispatch input '{name}' is not declared (declared inputs: {declared})")]
    UnknownInput {
        /// Supplied name.
        name: String,
        /// Stable comma-separated declared input list.
        declared: String,
    },
    /// A required dispatch input had neither an override nor a default.
    #[error("{span}: required workflow_dispatch input '{name}' has no value")]
    MissingRequiredInput {
        /// Input declaration location.
        span: greenlit_workflow::Span,
        /// Input name.
        name: String,
    },
    /// A supplied/default input value did not match its declared type.
    #[error("{span}: workflow_dispatch input '{name}' must be {expected}, got '{value}'")]
    InvalidInputValue {
        /// Input declaration location.
        span: greenlit_workflow::Span,
        /// Input name.
        name: String,
        /// Required domain.
        expected: String,
        /// Rejected value.
        value: String,
    },
    /// The selected synthetic event is not one of the workflow's triggers.
    #[error("{span}: workflow does not declare the `{event}` event")]
    EventNotDeclared {
        /// Workflow location.
        span: greenlit_workflow::Span,
        /// Selected event.
        event: &'static str,
    },
    /// The event is declared, but its branch/path/activity filters reject
    /// the locally synthesized event.
    #[error("{span}: synthetic `{event}` event does not satisfy its trigger filters ({detail})")]
    EventFilteredOut {
        /// Event-filter location.
        span: greenlit_workflow::Span,
        /// Selected event.
        event: &'static str,
        /// Stable description of the synthetic values that were tested.
        detail: String,
    },
    /// A parsed filter pattern could not be compiled by the event matcher.
    #[error("{span}: invalid `{axis}` filter pattern '{pattern}': {message}")]
    InvalidFilterPattern {
        /// Pattern location.
        span: greenlit_workflow::Span,
        /// Filter axis (`branches`, `paths`, ...).
        axis: &'static str,
        /// Pattern as written, without a leading exclusion marker.
        pattern: String,
        /// Regex compilation/grammar diagnostic.
        message: String,
    },
}

/// Builds a synthetic event, reading local git metadata rooted at
/// `repo_root`. `dispatch_inputs` supplies `workflow_dispatch` input
/// overrides by name (from `-s`/`--var`-style local overrides — resolving
/// *which* value wins is the CLI's job; this function only consumes the
/// already-resolved map), used only when `kind` is
/// [`EventKind::WorkflowDispatch`].
pub fn build_synthetic_event(
    kind: EventKind,
    repo_root: &Path,
    workflow: &Workflow,
    dispatch_inputs: &HashMap<String, String>,
) -> Result<SyntheticEvent, EventError> {
    if kind != EventKind::WorkflowDispatch && !dispatch_inputs.is_empty() {
        return Err(EventError::InputsForNonDispatch {
            event: kind.event_name(),
        });
    }
    let selected_triggers: Vec<&Trigger> = workflow
        .on
        .iter()
        .map(|trigger| &trigger.value)
        .filter(|trigger| trigger_declares(trigger, kind))
        .collect();
    let has_unsupported_workflow_call = workflow
        .on
        .iter()
        .any(|trigger| matches!(&trigger.value, Trigger::WorkflowCall(_)));
    if selected_triggers.is_empty() && !has_unsupported_workflow_call {
        return Err(EventError::EventNotDeclared {
            span: workflow.span.clone(),
            event: kind.event_name(),
        });
    }
    let git = collect_git_context(repo_root)?;
    if !selected_triggers.is_empty()
        && !selected_triggers
            .iter()
            .map(|trigger| webhook_filter_matches(trigger, kind, &git))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|matches| matches)
    {
        let span = selected_triggers
            .iter()
            .find_map(|trigger| match trigger {
                Trigger::Webhook { filter, .. } => Some(filter.span.clone()),
                _ => None,
            })
            .unwrap_or_else(|| workflow.span.clone());
        let detail = match kind {
            EventKind::Push => format!(
                "branch '{}', {} compared path(s){}",
                git.branch,
                git.changed_paths.len(),
                if git.changed_paths_truncated {
                    "; comparison truncated at GitHub's 3,000-path limit"
                } else {
                    ""
                }
            ),
            EventKind::PullRequest => format!(
                "base branch '{}', activity 'opened', {} compared path(s){}",
                git.pull_request_base_branch,
                git.pull_request_changed_paths.len(),
                if git.pull_request_changed_paths_truncated {
                    "; comparison truncated at GitHub's 3,000-path limit"
                } else {
                    ""
                }
            ),
            EventKind::WorkflowDispatch => "manual dispatch".to_string(),
        };
        return Err(EventError::EventFilteredOut {
            span,
            event: kind.event_name(),
            detail,
        });
    }
    let workflow_path = workflow_path_relative_to(repo_root, &workflow.span.file);
    let workflow_file_matches_head = file_matches_head(repo_root, &workflow_path)?;
    let workflow_name = workflow
        .name
        .as_ref()
        .map(|n| n.value.clone())
        .unwrap_or_else(|| workflow_path.clone());

    let (event_payload, extra_top_level, inputs) = match kind {
        EventKind::Push => (
            push_event_payload(&git),
            branch_ref_fields(&git.branch),
            Value::object(vec![]),
        ),
        EventKind::PullRequest => pull_request_payload(&git),
        EventKind::WorkflowDispatch => {
            workflow_dispatch_payload(workflow, dispatch_inputs, &git.branch)?
        }
    };

    let (workflow_ref, mut deferred_github_properties) = match kind {
        EventKind::PullRequest => (
            format!(
                "{}/{}@refs/pull/{SYNTHETIC_PULL_REQUEST_NUMBER}/merge",
                git.repository, workflow_path
            ),
            BTreeSet::from(["sha".to_string(), "workflow_sha".to_string()]),
        ),
        EventKind::Push | EventKind::WorkflowDispatch => (
            format!(
                "{}/{}@refs/heads/{}",
                git.repository, workflow_path, git.branch
            ),
            BTreeSet::new(),
        ),
    };
    if !workflow_file_matches_head {
        deferred_github_properties.insert("workflow_sha".to_string());
    }

    let mut github_entries = vec![
        (
            "event_name".to_string(),
            Value::String(kind.event_name().to_string()),
        ),
        (
            "repository".to_string(),
            Value::String(git.repository.clone()),
        ),
        (
            "repository_owner".to_string(),
            Value::String(git.repository_owner.clone()),
        ),
        ("actor".to_string(), Value::String(git.actor.clone())),
        (
            "triggering_actor".to_string(),
            Value::String(git.actor.clone()),
        ),
        ("workflow".to_string(), Value::String(workflow_name)),
        ("workflow_ref".to_string(), Value::String(workflow_ref)),
        (
            "server_url".to_string(),
            Value::String("https://github.com".to_string()),
        ),
        (
            "api_url".to_string(),
            Value::String("https://api.github.com".to_string()),
        ),
        (
            "graphql_url".to_string(),
            Value::String("https://api.github.com/graphql".to_string()),
        ),
        ("event".to_string(), event_payload),
    ];
    if kind != EventKind::PullRequest {
        github_entries.push(("sha".to_string(), Value::String(git.sha.clone())));
    }
    if kind != EventKind::PullRequest && workflow_file_matches_head {
        github_entries.push(("workflow_sha".to_string(), Value::String(git.sha.clone())));
    }
    github_entries.extend(extra_top_level);

    Ok(SyntheticEvent {
        kind,
        github: Value::object(github_entries),
        inputs,
        deferred_github_properties,
    })
}

/// GitHub exposes the unnamed workflow and `github.workflow_ref` using a
/// repository-relative workflow path, never the caller's absolute path.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#github-context
fn workflow_path_relative_to(repo_root: &Path, source: &str) -> String {
    let path = Path::new(source);
    let relative = if path.is_absolute() {
        path.strip_prefix(repo_root).unwrap_or(path)
    } else {
        path
    };
    relative.to_string_lossy().replace('\\', "/")
}

fn trigger_declares(trigger: &Trigger, kind: EventKind) -> bool {
    match (trigger, kind) {
        (Trigger::Webhook { name, .. }, EventKind::Push) => name == "push",
        (Trigger::Webhook { name, .. }, EventKind::PullRequest) => name == "pull_request",
        (Trigger::WorkflowDispatch(_), EventKind::WorkflowDispatch) => true,
        _ => false,
    }
}
