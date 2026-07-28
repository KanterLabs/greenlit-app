use std::path::Path;
use std::process::{Command, Stdio};

use super::{MAX_PATH_BYTES, SourceSnapshotError, remote};

const MAX_PATHS: usize = 1_000_000;

pub(super) fn clone_git_metadata(
    repo_root: &Path,
    destination: &Path,
) -> Result<(), SourceSnapshotError> {
    let original_origin = git_optional_text(repo_root, &["config", "--get", "remote.origin.url"])?
        .map(|origin| remote::credential_free_identity(&origin))
        .transpose()
        .map_err(|()| SourceSnapshotError::UnsafeRemote)?;
    let output = private_git_command()
        // Do not copy user- or system-supplied Git templates into retained
        // evidence. In particular, sample hooks preserve executable modes;
        // the child-local private umask below makes Git's generated metadata
        // files 0600 and directories 0700 from their first creation.
        .args([
            "clone",
            "--template=",
            "--no-hardlinks",
            "--no-checkout",
            "--no-tags",
            "--quiet",
            "--",
        ])
        .arg(repo_root)
        .arg(destination)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| git_error(&["clone"], error.to_string()))?;
    if output.status.success() {
        match original_origin {
            Some(origin) => {
                git_output(destination, &["remote", "set-url", "origin", &origin])?;
            }
            None => {
                // A local clone invents an origin pointing back at the live
                // checkout. Do not retain that host path when the source
                // repository itself had no configured remote identity.
                git_output(destination, &["remote", "remove", "origin"])?;
            }
        }
        Ok(())
    } else {
        Err(git_error(
            &["clone"],
            bounded_stderr(&output.stderr, output.status.to_string()),
        ))
    }
}

pub(super) fn git_text(repo_root: &Path, args: &[&str]) -> Result<String, SourceSnapshotError> {
    let output = git_output(repo_root, args)?;
    String::from_utf8(output)
        .map(|value| value.trim().to_string())
        .map_err(|error| git_error(args, format!("stdout was not UTF-8: {error}")))
}

fn git_optional_text(
    repo_root: &Path,
    args: &[&str],
) -> Result<Option<String>, SourceSnapshotError> {
    let output = private_git_command()
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| git_error(args, error.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }
    String::from_utf8(output.stdout)
        .map(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .map_err(|error| git_error(args, format!("stdout was not UTF-8: {error}")))
}

pub(super) fn git_status(repo_root: &Path) -> Result<Vec<u8>, SourceSnapshotError> {
    git_output(
        repo_root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
}

pub(super) fn git_paths(
    repo_root: &Path,
    args: &[&str],
) -> Result<Vec<String>, SourceSnapshotError> {
    let bytes = git_output(repo_root, args)?;
    let mut paths = Vec::new();
    for raw in bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        if paths.len() == MAX_PATHS {
            return Err(SourceSnapshotError::PathLimit { limit: MAX_PATHS });
        }
        if raw.len() > MAX_PATH_BYTES {
            return Err(SourceSnapshotError::Io {
                path: repo_root.display().to_string(),
                message: format!("one source path exceeds {MAX_PATH_BYTES} bytes"),
            });
        }
        let path = std::str::from_utf8(raw).map_err(|_| SourceSnapshotError::NonUtf8Path)?;
        if path == ".litci" || path.starts_with(".litci/") {
            continue;
        }
        paths.push(path.to_string());
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn git_output(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>, SourceSnapshotError> {
    let output = private_git_command()
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| git_error(args, error.to_string()))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(git_error(
            args,
            bounded_stderr(&output.stderr, output.status.to_string()),
        ))
    }
}

fn private_git_command() -> Command {
    // Git may create lockfiles and replace metadata even for configuration
    // updates. A child-local umask keeps every such inode private without
    // changing the caller process's global umask. Arguments remain separate
    // process arguments and are never interpolated into shell source.
    let mut command = Command::new("sh");
    command
        .args(["-c", "umask 077; exec \"$@\"", "greenlit-private-git"])
        .arg("git");
    command
}

fn bounded_stderr(stderr: &[u8], fallback: String) -> String {
    let retained = &stderr[..stderr.len().min(64 * 1024)];
    let text = String::from_utf8_lossy(retained).trim().to_string();
    if text.is_empty() { fallback } else { text }
}

fn git_error(args: &[&str], message: String) -> SourceSnapshotError {
    SourceSnapshotError::Git {
        args: args.join(" "),
        message,
    }
}
