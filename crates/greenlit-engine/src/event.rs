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
//! meaningful without a real backend (queue/run identity — `run_id`,
//! `run_number`, `run_attempt` — is execution-phase data, so v0 uses fixed
//! placeholder values, called out at each use).

use std::collections::HashMap;
use std::path::Path;

use greenlit_expr::Value;
use greenlit_workflow::model::trigger::{
    Trigger, WorkflowDispatchInput, WorkflowDispatchInputType,
};
use greenlit_workflow::model::workflow::Workflow;

use crate::convert::yaml_value_to_value;
use crate::git::{GitContext, GitError, collect_git_context};

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
}

/// Everything that can go wrong building a synthetic event.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum EventError {
    /// Local git metadata could not be collected.
    #[error(transparent)]
    Git(#[from] GitError),
}

/// Fixed placeholder values for run-identity fields that only exist once a
/// real run starts (queue/execution-phase data, out of Phase 1 scope) —
/// `1` for all three, matching GitHub's own numbering starting point.
const PLACEHOLDER_RUN_ID: u64 = 1;
const PLACEHOLDER_RUN_NUMBER: u64 = 1;
const PLACEHOLDER_RUN_ATTEMPT: u64 = 1;

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
    let git = collect_git_context(repo_root)?;
    let workflow_name = workflow
        .name
        .as_ref()
        .map(|n| n.value.clone())
        .unwrap_or_else(|| workflow.span.file.to_string());

    let (event_payload, extra_top_level, inputs) = match kind {
        EventKind::Push => (
            push_event_payload(&git),
            branch_ref_fields(&git.branch),
            Value::object(vec![]),
        ),
        EventKind::PullRequest => pull_request_payload(&git),
        EventKind::WorkflowDispatch => {
            workflow_dispatch_payload(workflow, dispatch_inputs, &git.branch)
        }
    };

    let mut github_entries = vec![
        (
            "event_name".to_string(),
            Value::String(kind.event_name().to_string()),
        ),
        ("sha".to_string(), Value::String(git.sha.clone())),
        (
            "repository".to_string(),
            Value::String(git.repository.clone()),
        ),
        (
            "repository_owner".to_string(),
            Value::String(git.repository_owner.clone()),
        ),
        ("actor".to_string(), Value::String(git.actor.clone())),
        ("workflow".to_string(), Value::String(workflow_name)),
        (
            "run_id".to_string(),
            Value::Number(PLACEHOLDER_RUN_ID as f64),
        ),
        (
            "run_number".to_string(),
            Value::Number(PLACEHOLDER_RUN_NUMBER as f64),
        ),
        (
            "run_attempt".to_string(),
            Value::Number(PLACEHOLDER_RUN_ATTEMPT as f64),
        ),
        ("event".to_string(), event_payload),
    ];
    github_entries.extend(extra_top_level);

    Ok(SyntheticEvent {
        kind,
        github: Value::object(github_entries),
        inputs,
    })
}

fn push_event_payload(git: &GitContext) -> Value {
    // `github.ref`/`github.ref_name`/`github.ref_type` per the Contexts
    // reference's `github context` table; `before` is GitHub's documented
    // all-zero SHA sentinel when there is no parent commit (a brand-new
    // branch/initial commit).
    Value::object(vec![
        (
            "before".to_string(),
            Value::String(git.parent_sha.clone().unwrap_or_else(|| "0".repeat(40))),
        ),
        ("after".to_string(), Value::String(git.sha.clone())),
        (
            "ref".to_string(),
            Value::String(format!("refs/heads/{}", git.branch)),
        ),
    ])
}

/// GitHub Contexts reference, `github.ref`, `github.ref_name`, and
/// `github.ref_type`: a branch-backed push or manual dispatch uses the
/// fully formed `refs/heads/<name>` ref, the short branch name, and
/// `"branch"`, respectively.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#github-context
fn branch_ref_fields(branch: &str) -> Vec<(String, Value)> {
    vec![
        (
            "ref".to_string(),
            Value::String(format!("refs/heads/{branch}")),
        ),
        ("ref_name".to_string(), Value::String(branch.to_string())),
        ("ref_type".to_string(), Value::String("branch".to_string())),
    ]
}

fn pull_request_payload(git: &GitContext) -> (Value, Vec<(String, Value)>, Value) {
    // Synthetic placeholders: v0 has no real GitHub API to fetch an actual
    // open PR, so this models the most common shape (head == current
    // branch/sha, base == a placeholder "main", PR #1, action "opened")
    // rather than guessing at a specific real pull request's data.
    const PLACEHOLDER_PR_NUMBER: f64 = 1.0;
    let head_ref = git.branch.clone();
    let base_ref = "main".to_string();
    let payload = Value::object(vec![
        ("action".to_string(), Value::String("opened".to_string())),
        ("number".to_string(), Value::Number(PLACEHOLDER_PR_NUMBER)),
        (
            "pull_request".to_string(),
            Value::object(vec![
                ("number".to_string(), Value::Number(PLACEHOLDER_PR_NUMBER)),
                (
                    "head".to_string(),
                    Value::object(vec![
                        ("ref".to_string(), Value::String(head_ref.clone())),
                        ("sha".to_string(), Value::String(git.sha.clone())),
                    ]),
                ),
                (
                    "base".to_string(),
                    Value::object(vec![("ref".to_string(), Value::String(base_ref.clone()))]),
                ),
            ]),
        ),
    ]);
    let extra_top_level = vec![
        ("base_ref".to_string(), Value::String(base_ref)),
        ("head_ref".to_string(), Value::String(head_ref)),
        (
            "ref".to_string(),
            // "the PR merge branch" per the Contexts reference.
            Value::String(format!("refs/pull/{}/merge", PLACEHOLDER_PR_NUMBER as u64)),
        ),
        (
            "ref_name".to_string(),
            // Contexts reference: unmerged pull requests use
            // `<pr_number>/merge` as the short ref name.
            Value::String(format!("{}/merge", PLACEHOLDER_PR_NUMBER as u64)),
        ),
        ("ref_type".to_string(), Value::String("branch".to_string())),
    ];
    (payload, extra_top_level, Value::object(vec![]))
}

fn workflow_dispatch_payload(
    workflow: &Workflow,
    provided: &HashMap<String, String>,
    branch: &str,
) -> (Value, Vec<(String, Value)>, Value) {
    let declared_inputs: &[WorkflowDispatchInput] = workflow
        .on
        .iter()
        .find_map(|t| match &t.value {
            Trigger::WorkflowDispatch(wd) => Some(wd.inputs.as_slice()),
            _ => None,
        })
        .unwrap_or(&[]);

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
                    .map(|d| yaml_value_to_display(&d.value))
            })
            .unwrap_or_default();
        event_inputs.push((input.name.value.clone(), Value::String(raw.clone())));
        typed_inputs.push((input.name.value.clone(), typed_input_value(input, &raw)));
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
    (
        payload,
        branch_ref_fields(branch),
        Value::object(typed_inputs),
    )
}

fn yaml_value_to_display(v: &greenlit_workflow::model::value::YamlValue) -> String {
    greenlit_expr::value::to_display_string(&yaml_value_to_value(v))
}

/// `github.event.inputs.*` is always a string (design memo/docs:
/// "`github.event.inputs` always gives strings"); the top-level `inputs`
/// context reflects the declared `type:` — boolean/number get a genuine
/// typed value, everything else (including `choice`/`environment`, which
/// are string-shaped) stays a string.
fn typed_input_value(input: &WorkflowDispatchInput, raw: &str) -> Value {
    match input.input_type.as_ref().map(|t| t.value) {
        Some(WorkflowDispatchInputType::Boolean) => Value::Bool(raw.eq_ignore_ascii_case("true")),
        Some(WorkflowDispatchInputType::Number) => Value::Number(raw.parse::<f64>().unwrap_or(0.0)),
        _ => Value::String(raw.to_string()),
    }
}
