//! Local git-plumbing metadata collection for the synthetic event builder
//! (`crate::event`).
//!
//! `PHASE-1-engine-core.md`'s greenlit-engine section: "Synthetic event
//! builder: default `push` event populated from local git metadata (repo
//! name, branch, SHA, actor from git config)". Every value here is read by
//! shelling out to the `git` binary's plumbing subcommands (`rev-parse`,
//! `symbolic-ref`, `config`) rather than a linked git library: v0 has no
//! other git dependency, and adding one (`git2`, which itself links
//! `libgit2`) purely to read four values that `git` itself already exposes
//! as stable, scriptable plumbing output would be disproportionate. No
//! network calls are made — every subcommand used here is local-only.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Everything the synthetic event builder needs from the local git
/// checkout: enough to populate `github.repository`, `github.ref`,
/// `github.sha`, and `github.actor` (design memo's `github` context roots)
/// without a real GitHub event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitContext {
    /// `owner/repo` parsed from the `origin` remote URL, or (when there is
    /// no `origin` remote — e.g. a fresh local-only repo) the repository
    /// root directory's base name, with no owner segment.
    pub repository: String,
    /// The owner segment of [`GitContext::repository`], or `"local"` when
    /// there was no remote to parse one from.
    pub repository_owner: String,
    /// The current branch's short name (e.g. `"main"`), from
    /// `git symbolic-ref --short HEAD`. Detached-HEAD checkouts are not
    /// separately modeled in v0 — see [`GitError::DetachedHead`].
    pub branch: String,
    /// The full 40-character hex SHA of `HEAD`.
    pub sha: String,
    /// The parent commit's SHA, when `HEAD` has one, else `None` (the
    /// repository's very first commit) — used to synthesize a push event's
    /// `before` field.
    pub parent_sha: Option<String>,
    /// `git config user.name`, falling back to `git config user.email`,
    /// falling back to the literal `"local"` when neither is configured.
    pub actor: String,
}

/// A failure collecting local git metadata.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum GitError {
    /// `path` is not inside a git working tree at all (`git rev-parse
    /// --show-toplevel` failed) — there is no repository to read metadata
    /// from.
    #[error("{path}: not a git repository (needed to build a synthetic event)")]
    NotARepository {
        /// The path that was checked.
        path: String,
    },
    /// The repository has no commits yet, so there is no `HEAD` to read a
    /// SHA from.
    #[error("{path}: git repository has no commits yet (HEAD is unborn)")]
    NoCommits {
        /// The repository root.
        path: String,
    },
    /// `HEAD` is detached (not on any branch) — v0's synthetic push event
    /// needs a branch name for `github.ref`/`github.ref_name`.
    #[error(
        "{path}: HEAD is detached (not on a branch); check out a branch to build a synthetic push event"
    )]
    DetachedHead {
        /// The repository root.
        path: String,
    },
    /// A `git` subcommand could not even be spawned (binary missing from
    /// `PATH`, permissions, …).
    #[error("failed to run 'git {args}': {message}")]
    CommandFailed {
        /// The subcommand and arguments that were attempted, space-joined.
        args: String,
        /// The underlying I/O error's message.
        message: String,
    },
}

/// Runs `git <args>` rooted at `repo_root`, returning trimmed stdout on
/// success (exit code 0) or `None` on any non-zero exit (the plumbing
/// subcommands used here signal "no such ref"/"no remote" this way, which
/// is a normal, expected outcome — not itself an error).
fn run_git(repo_root: &Path, args: &[&str]) -> Result<Option<String>, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|e| GitError::CommandFailed {
            args: args.join(" "),
            message: e.to_string(),
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

/// Parses `owner/repo` out of a git remote URL, honoring both the SSH
/// shorthand (`git@github.com:owner/repo.git`) and HTTPS
/// (`https://github.com/owner/repo.git`, with or without a trailing `.git`)
/// forms — the two GitHub emits for `origin` depending on how the repo was
/// cloned.
pub(crate) fn parse_owner_repo(remote_url: &str) -> Option<(String, String)> {
    let without_suffix = remote_url.strip_suffix(".git").unwrap_or(remote_url);
    let path_part = if let Some(rest) = without_suffix.strip_prefix("git@") {
        rest.split_once(':').map(|(_, path)| path)?
    } else if let Some(rest) = without_suffix
        .strip_prefix("https://")
        .or_else(|| without_suffix.strip_prefix("http://"))
    {
        rest.split_once('/').map(|(_, path)| path)?
    } else if let Some(rest) = without_suffix.strip_prefix("ssh://") {
        // ssh://git@github.com/owner/repo(.git)
        rest.split_once('/')?.1
    } else {
        return None;
    };
    let (owner, repo) = path_part.rsplit_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// Collects [`GitContext`] from the repository containing `repo_root`.
pub fn collect_git_context(repo_root: &Path) -> Result<GitContext, GitError> {
    let path_display = repo_root.display().to_string();

    let toplevel = run_git(repo_root, &["rev-parse", "--show-toplevel"])?.ok_or_else(|| {
        GitError::NotARepository {
            path: path_display.clone(),
        }
    })?;
    let toplevel = PathBuf::from(toplevel);

    let sha = run_git(&toplevel, &["rev-parse", "HEAD"])?.ok_or_else(|| GitError::NoCommits {
        path: path_display.clone(),
    })?;

    let branch = run_git(&toplevel, &["symbolic-ref", "--short", "HEAD"])?.ok_or_else(|| {
        GitError::DetachedHead {
            path: path_display.clone(),
        }
    })?;

    let parent_sha = run_git(&toplevel, &["rev-parse", "HEAD~1"])?;

    let remote_url = run_git(&toplevel, &["config", "--get", "remote.origin.url"])?;
    let (repository, repository_owner) = match remote_url.as_deref().and_then(parse_owner_repo) {
        Some((owner, repo)) => (format!("{owner}/{repo}"), owner),
        None => {
            let dir_name = toplevel
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".to_string());
            (dir_name, "local".to_string())
        }
    };

    let actor = run_git(&toplevel, &["config", "user.name"])?
        .or(run_git(&toplevel, &["config", "user.email"])?)
        .unwrap_or_else(|| "local".to_string());

    Ok(GitContext {
        repository,
        repository_owner,
        branch,
        sha,
        parent_sha,
        actor,
    })
}
