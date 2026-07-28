//! Synthetic push and pull-request payload shapes shared by the event
//! builder and manual-dispatch builder.
//!
//! Synthetic placeholders throughout: v0 has no real GitHub API or webhook
//! delivery, so each payload only ever carries the fields this crate can
//! honestly derive from local git metadata — `repository.{full_name,name,
//! owner.login}` and `sender.login` ([`repository_and_sender`]) on top of
//! the event-specific fields each builder below already had. GitHub's much
//! larger real payloads (ids, avatar URLs, timestamps, a `commits` array, a
//! PR body, ...) are deliberately left out rather than fabricated.

use greenlit_expr::Value;

use super::{EventPayload, SYNTHETIC_PULL_REQUEST_NUMBER};
use crate::git::GitContext;

/// The `repository`/`sender` fields every real GitHub webhook payload
/// carries at its top level, populated from local git metadata. Shared by
/// every synthetic event kind so `push`/`pull_request` stay consistent with
/// each other rather than each re-deriving `repository.name` from
/// [`GitContext::repository`] independently.
fn repository_and_sender(git: &GitContext) -> Vec<(String, Value)> {
    let name = git
        .repository
        .rsplit('/')
        .next()
        .unwrap_or(&git.repository)
        .to_string();
    vec![
        (
            "repository".to_string(),
            Value::object(vec![
                (
                    "full_name".to_string(),
                    Value::String(git.repository.clone()),
                ),
                ("name".to_string(), Value::String(name)),
                (
                    "owner".to_string(),
                    Value::object(vec![(
                        "login".to_string(),
                        Value::String(git.repository_owner.clone()),
                    )]),
                ),
            ]),
        ),
        (
            "sender".to_string(),
            Value::object(vec![(
                "login".to_string(),
                Value::String(git.actor.clone()),
            )]),
        ),
    ]
}

pub(super) fn push_event_payload(git: &GitContext) -> Value {
    // `github.ref`/`github.ref_name`/`github.ref_type` per the Contexts
    // reference's `github context` table; `before` is GitHub's documented
    // all-zero SHA sentinel when there is no parent commit.
    let mut entries = vec![
        (
            "before".to_string(),
            Value::String(git.parent_sha.clone().unwrap_or_else(|| "0".repeat(40))),
        ),
        ("after".to_string(), Value::String(git.sha.clone())),
        (
            "ref".to_string(),
            Value::String(format!("refs/heads/{}", git.branch)),
        ),
    ];
    entries.extend(repository_and_sender(git));
    Value::object(entries)
}

/// A branch-backed push or manual dispatch uses the fully formed ref, the
/// short branch name, and `"branch"`, respectively.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#github-context
pub(super) fn branch_ref_fields(branch: &str) -> Vec<(String, Value)> {
    vec![
        (
            "ref".to_string(),
            Value::String(format!("refs/heads/{branch}")),
        ),
        ("ref_name".to_string(), Value::String(branch.to_string())),
        ("ref_type".to_string(), Value::String("branch".to_string())),
    ]
}

pub(super) fn pull_request_payload(git: &GitContext) -> EventPayload {
    // Synthetic placeholders: v0 has no real GitHub API to fetch an actual
    // open PR, so this models the most common shape (head == current
    // branch/sha, locally-derived base, PR #1, action `opened`). GitHub's
    // payload exposes the PR's base and head under these fields.
    // https://docs.github.com/en/webhooks/webhook-events-and-payloads#pull_request
    let placeholder_pr_number = SYNTHETIC_PULL_REQUEST_NUMBER as f64;
    let head_ref = git.branch.clone();
    let base_ref = git.pull_request_base_branch.clone();
    let mut payload_entries = vec![
        ("action".to_string(), Value::String("opened".to_string())),
        ("number".to_string(), Value::Number(placeholder_pr_number)),
        (
            "pull_request".to_string(),
            Value::object(vec![
                ("number".to_string(), Value::Number(placeholder_pr_number)),
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
    ];
    payload_entries.extend(repository_and_sender(git));
    let payload = Value::object(payload_entries);
    let extra_top_level = vec![
        ("base_ref".to_string(), Value::String(base_ref)),
        ("head_ref".to_string(), Value::String(head_ref)),
        (
            "ref".to_string(),
            Value::String(format!("refs/pull/{SYNTHETIC_PULL_REQUEST_NUMBER}/merge")),
        ),
        (
            "ref_name".to_string(),
            Value::String(format!("{SYNTHETIC_PULL_REQUEST_NUMBER}/merge")),
        ),
        ("ref_type".to_string(), Value::String("branch".to_string())),
    ];
    (payload, extra_top_level, Value::object(vec![]))
}
