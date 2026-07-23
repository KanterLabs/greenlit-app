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

mod base;
mod process;

/// Everything the synthetic event builder needs from the local git
/// checkout: enough to populate `github.repository`, `github.ref`,
/// `github.sha`, and `github.actor` without a real GitHub event payload, as
/// required by `PHASE-1-engine-core.md`'s synthetic-event task.
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
    /// Paths changed by `HEAD`, relative to the repository root. Synthetic
    /// push trigger path filters use this deterministic local change set.
    pub changed_paths: Vec<String>,
    /// Whether [`GitContext::changed_paths`] reached GitHub's 3,000-path
    /// comparison boundary and further paths were deliberately not retained.
    pub changed_paths_truncated: bool,
    /// The local branch used as the synthetic pull request's base branch.
    pub pull_request_base_branch: String,
    /// Paths changed between the merge base of
    /// [`GitContext::pull_request_base_branch`] and `HEAD`, relative to the
    /// repository root.
    pub pull_request_changed_paths: Vec<String>,
    /// Whether [`GitContext::pull_request_changed_paths`] reached GitHub's
    /// 3,000-path comparison boundary and further paths were deliberately not
    /// retained.
    pub pull_request_changed_paths_truncated: bool,
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
    /// A required plumbing command exited unsuccessfully.
    #[error("required 'git {args}' command failed: {message}")]
    CommandUnsuccessful {
        /// The subcommand and arguments that failed.
        args: String,
        /// Git's bounded diagnostic or exit status.
        message: String,
    },
    /// A partial/promisor clone lacks an object required for truthful local
    /// metadata, and Greenlit deliberately refused Git's lazy network fetch.
    #[error("local repository is missing Git objects required by 'git {args}'")]
    MissingObjects {
        /// The read-only plumbing command that needed the object.
        args: String,
    },
    /// A successful plumbing command produced more scalar stdout than can be
    /// safely interpreted as one metadata value.
    #[error("Git stdout from 'git {args}' exceeds the {max_bytes}-byte safety limit")]
    OutputLimit {
        /// The bounded local plumbing command.
        args: String,
        /// Maximum captured stdout bytes.
        max_bytes: usize,
    },
    /// A NUL-delimited changed path exceeded the per-record memory bound.
    #[error("a changed path from 'git {args}' exceeds the {max_bytes}-byte safety limit")]
    ChangedPathLimit {
        /// The bounded local diff command.
        args: String,
        /// Maximum bytes accepted for one changed path.
        max_bytes: usize,
    },
    /// A local Git plumbing process did not complete within its fixed
    /// deadline and was stopped.
    #[error("'git {args}' exceeded the {seconds}-second local command deadline and was stopped")]
    CommandTimedOut {
        /// The bounded local plumbing command.
        args: String,
        /// Deadline in seconds.
        seconds: u64,
    },
}

fn missing_object_diagnostic(stderr: &str) -> bool {
    let diagnostic = stderr.to_ascii_lowercase();
    [
        "could not fetch",
        "promisor remote",
        "missing blob",
        "missing tree",
        "bad object",
        "unable to read tree",
        "invalid object",
    ]
    .iter()
    .any(|needle| diagnostic.contains(needle))
}

fn failed_command(
    args: &[&str],
    stderr: &[u8],
    stderr_truncated: bool,
    status: std::process::ExitStatus,
) -> GitError {
    let mut diagnostic = String::from_utf8_lossy(stderr).trim().to_string();
    if stderr_truncated {
        diagnostic.push_str(" [diagnostic truncated at 65536 bytes]");
    }
    if missing_object_diagnostic(&diagnostic) {
        return GitError::MissingObjects {
            args: args.join(" "),
        };
    }
    GitError::CommandUnsuccessful {
        args: args.join(" "),
        message: if diagnostic.is_empty() {
            status.to_string()
        } else {
            diagnostic
        },
    }
}

/// Runs an optional Git query. Non-zero means an absent optional ref/config
/// unless Git explicitly reports a missing object, which is never absence.
fn run_git(repo_root: &Path, args: &[&str]) -> Result<Option<String>, GitError> {
    let output = process::run_text(repo_root, args)?;
    if !output.status.success() {
        if missing_object_diagnostic(&String::from_utf8_lossy(&output.stderr)) {
            return Err(GitError::MissingObjects {
                args: args.join(" "),
            });
        }
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.value).trim().to_string();
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

/// Finds the root of the git worktree containing `path`.
///
/// This delegates to `git rev-parse --show-toplevel`, so worktrees and
/// submodules follow git's own discovery rules.
pub fn find_repository_root(path: &Path) -> Result<PathBuf, GitError> {
    let path_display = path.display().to_string();
    run_git(path, &["rev-parse", "--show-toplevel"])?
        .map(PathBuf::from)
        .ok_or(GitError::NotARepository { path: path_display })
}

/// Returns whether the working-tree file at `repo_relative` has exactly the
/// blob content recorded for that path in `HEAD`.
///
/// A synthetic event can name `HEAD` as `github.workflow_sha` only when the
/// parsed workflow really came from that commit. Untracked, staged, and
/// unstaged workflow edits therefore return `false` rather than acquiring a
/// misleading commit identity.
pub(crate) fn file_matches_head(repo_root: &Path, repo_relative: &str) -> Result<bool, GitError> {
    let head_spec = format!("HEAD:{repo_relative}");
    let head_hash = run_git(repo_root, &["rev-parse", "--verify", &head_spec])?;
    let working_hash = run_git(repo_root, &["hash-object", "--", repo_relative])?;
    Ok(head_hash
        .zip(working_hash)
        .is_some_and(|(head, working)| head == working))
}

fn changed_paths(repo_root: &Path, range: &[&str]) -> Result<(Vec<String>, bool), GitError> {
    let output = process::run_changed_paths(repo_root, range)?;
    let (paths, truncated) = output.value;
    if !truncated && !output.status.success() {
        return Err(failed_command(
            range,
            &output.stderr,
            output.stderr_truncated,
            output.status,
        ));
    }
    Ok((paths, truncated))
}

/// Collects [`GitContext`] from the repository containing `repo_root`.
pub fn collect_git_context(repo_root: &Path) -> Result<GitContext, GitError> {
    let path_display = repo_root.display().to_string();

    let toplevel = find_repository_root(repo_root).map_err(|error| match error {
        GitError::NotARepository { .. } => GitError::NotARepository {
            path: path_display.clone(),
        },
        other => other,
    })?;

    let sha = run_git(&toplevel, &["rev-parse", "HEAD"])?.ok_or_else(|| GitError::NoCommits {
        path: path_display.clone(),
    })?;

    let branch = run_git(&toplevel, &["symbolic-ref", "--short", "HEAD"])?.ok_or_else(|| {
        GitError::DetachedHead {
            path: path_display.clone(),
        }
    })?;

    let parent_sha = run_git(&toplevel, &["rev-parse", "HEAD~1"])?;
    let (push_changed_paths, push_changed_paths_truncated) = changed_paths(
        &toplevel,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-z",
            "--no-ext-diff",
            "-r",
            "--root",
            "HEAD",
        ],
    )?;
    let (pull_request_base_branch, pull_request_merge_base) =
        base::pull_request_base(&toplevel, &branch)?;
    // GitHub evaluates pull-request path filters with a three-dot diff from
    // the merge base to the topic branch's latest commit, unlike a synthetic
    // push, which describes only `HEAD` here.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#git-diff-comparisons
    let pull_request_range = format!("{pull_request_merge_base}...HEAD");
    let (pull_request_changed_paths, pull_request_changed_paths_truncated) = changed_paths(
        &toplevel,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            &pull_request_range,
        ],
    )?;

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
        changed_paths: push_changed_paths,
        changed_paths_truncated: push_changed_paths_truncated,
        pull_request_base_branch,
        pull_request_changed_paths,
        pull_request_changed_paths_truncated,
    })
}
