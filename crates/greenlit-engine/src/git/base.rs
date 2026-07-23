//! Selection of a truthful local base for synthetic pull-request events.

use super::{GitError, run_git};
use std::path::Path;

fn upstream_candidate(
    repo_root: &Path,
    branch: &str,
) -> Result<Option<(String, String, Option<String>)>, GitError> {
    let Some(revision) = run_git(
        repo_root,
        &["rev-parse", "--symbolic-full-name", "@{upstream}"],
    )?
    else {
        return Ok(None);
    };
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    let remote = run_git(repo_root, &["config", "--get", &remote_key])?;
    let merge = run_git(repo_root, &["config", "--get", &merge_key])?;
    let branch_name = merge
        .as_deref()
        .and_then(|name| name.strip_prefix("refs/heads/"))
        .map(str::to_string)
        .or_else(|| {
            remote.as_deref().and_then(|remote_name| {
                revision
                    .strip_prefix(&format!("refs/remotes/{remote_name}/"))
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| branch.to_string());
    Ok(Some((branch_name, revision, remote)))
}

fn remote_head_candidate(
    repo_root: &Path,
    remote: &str,
) -> Result<Option<(String, String)>, GitError> {
    if remote == "." || remote.is_empty() {
        return Ok(None);
    }
    let head = format!("refs/remotes/{remote}/HEAD");
    let Some(revision) = run_git(repo_root, &["symbolic-ref", "--quiet", &head])? else {
        return Ok(None);
    };
    let prefix = format!("refs/remotes/{remote}/");
    Ok(revision
        .strip_prefix(&prefix)
        .map(|branch| (branch.to_string(), revision.clone())))
}

pub(super) fn pull_request_base(
    repo_root: &Path,
    branch: &str,
) -> Result<(String, String), GitError> {
    let upstream = upstream_candidate(repo_root, branch)?;
    let mut candidates = Vec::new();

    // A non-self upstream is the strongest local signal that a topic branch
    // was created from a specific integration branch. A same-named tracking
    // branch is held as a later fallback because it normally represents the
    // topic branch's remote copy, not its pull-request base.
    if let Some((name, revision, _)) = &upstream
        && name != branch
    {
        candidates.push((name.clone(), revision.clone()));
    }

    let configured_remote = upstream
        .as_ref()
        .and_then(|(_, _, remote)| remote.as_deref())
        .filter(|remote| *remote != ".")
        .unwrap_or("origin");
    if let Some(candidate) = remote_head_candidate(repo_root, configured_remote)? {
        candidates.push(candidate);
    }
    if configured_remote != "origin"
        && let Some(candidate) = remote_head_candidate(repo_root, "origin")?
    {
        candidates.push(candidate);
    }

    // Local-only repositories have no remote HEAD. `main`, then `master`,
    // are stable conventional fallbacks; if neither exists, the same-named
    // upstream or current branch still produces a deterministic synthetic
    // event rather than inventing a branch that is absent locally.
    for conventional in ["main", "master"] {
        let revision = format!("refs/heads/{conventional}");
        if run_git(repo_root, &["rev-parse", "--verify", &revision])?.is_some() {
            candidates.push((conventional.to_string(), revision));
        }
    }
    if let Some((name, revision, _)) = upstream {
        candidates.push((name, revision));
    }
    candidates.push((branch.to_string(), format!("refs/heads/{branch}")));

    for (name, revision) in candidates {
        if let Some(merge_base) = run_git(repo_root, &["merge-base", &revision, "HEAD"])? {
            return Ok((name, merge_base));
        }
    }

    // The current branch candidate always shares `HEAD`; this is defensive
    // for a concurrently modified repository rather than a reachable normal
    // git state.
    Ok((branch.to_string(), "HEAD".to_string()))
}
