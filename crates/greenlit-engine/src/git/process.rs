//! Bounded, deadline-enforced execution for local Git plumbing commands.

use super::GitError;
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_CHANGED_PATHS: usize = 3_000;
const MAX_CHANGED_PATH_BYTES: usize = 64 * 1024;
const MAX_GIT_STDOUT_BYTES: usize = 64 * 1024;
const MAX_GIT_STDERR_BYTES: usize = 64 * 1024;
const GIT_COMMAND_TIMEOUT_SECONDS: u64 = 5;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) struct GitProcessOutput<T> {
    pub(super) status: ExitStatus,
    pub(super) value: T,
    pub(super) stderr: Vec<u8>,
    pub(super) stderr_truncated: bool,
}

struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

pub(super) fn run_text(
    repo_root: &Path,
    args: &[&str],
) -> Result<GitProcessOutput<Vec<u8>>, GitError> {
    let args_text = args.join(" ");
    let output = execute_git(repo_root, args, move |stdout| {
        read_bounded(stdout, MAX_GIT_STDOUT_BYTES, &args_text, "stdout")
    })?;
    if output.value.truncated {
        return Err(GitError::OutputLimit {
            args: args.join(" "),
            max_bytes: MAX_GIT_STDOUT_BYTES,
        });
    }
    Ok(GitProcessOutput {
        status: output.status,
        value: output.value.bytes,
        stderr: output.stderr,
        stderr_truncated: output.stderr_truncated,
    })
}

pub(super) fn run_changed_paths(
    repo_root: &Path,
    args: &[&str],
) -> Result<GitProcessOutput<(Vec<String>, bool)>, GitError> {
    let args_text = args.join(" ");
    execute_git(repo_root, args, move |stdout| {
        read_changed_paths(stdout, &args_text)
    })
}

fn execute_git<T>(
    repo_root: &Path,
    args: &[&str],
    stdout_task: impl FnOnce(ChildStdout) -> Result<T, GitError> + Send + 'static,
) -> Result<GitProcessOutput<T>, GitError>
where
    T: Send + 'static,
{
    let mut child = git_command(repo_root, args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| command_failed(args, format!("could not start Git: {error}")))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            return Err(abort_setup::<T>(
                &mut child,
                args,
                "could not capture Git stdout".to_string(),
                None,
            ));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            return Err(abort_setup::<T>(
                &mut child,
                args,
                "could not capture Git stderr".to_string(),
                None,
            ));
        }
    };

    let stdout_reader = match std::thread::Builder::new()
        .name("litci-git-stdout".to_string())
        .spawn(move || stdout_task(stdout))
    {
        Ok(reader) => reader,
        Err(error) => {
            return Err(abort_setup::<T>(
                &mut child,
                args,
                format!("could not start Git stdout reader: {error}"),
                None,
            ));
        }
    };

    let stderr_args = args.join(" ");
    let stderr_reader = match std::thread::Builder::new()
        .name("litci-git-stderr".to_string())
        .spawn(move || read_bounded(stderr, MAX_GIT_STDERR_BYTES, &stderr_args, "stderr"))
    {
        Ok(reader) => reader,
        Err(error) => {
            return Err(abort_setup(
                &mut child,
                args,
                format!("could not start Git stderr reader: {error}"),
                Some(stdout_reader),
            ));
        }
    };

    let completion = wait_for_git(&mut child, args);
    let stdout_result = join_reader(stdout_reader, args, "stdout");
    let stderr_result = join_reader(stderr_reader, args, "stderr");
    let (status, timed_out) = completion?;
    let value = stdout_result?;
    let stderr = stderr_result?;
    if timed_out {
        return Err(GitError::CommandTimedOut {
            args: args.join(" "),
            seconds: GIT_COMMAND_TIMEOUT_SECONDS,
        });
    }
    Ok(GitProcessOutput {
        status,
        value,
        stderr: stderr.bytes,
        stderr_truncated: stderr.truncated,
    })
}

fn git_command(repo_root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo_root)
        .args(args)
        // Partial clones normally lazy-fetch a missing promisor object even
        // for read-only plumbing. Phase 1 is network-free, so every Git
        // subprocess opts out at the process boundary.
        // https://git-scm.com/docs/git#Documentation/git.txt-codeGITNOLAZYFETCHcode
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0");
    command
}

fn read_bounded(
    mut reader: impl Read,
    max_bytes: usize,
    args: &str,
    stream: &'static str,
) -> Result<BoundedCapture, GitError> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut truncated = false;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|error| GitError::CommandFailed {
                args: args.to_string(),
                message: format!("could not read Git {stream}: {error}"),
            })?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(bytes.len());
        let retained = remaining.min(read);
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < read;
    }
    Ok(BoundedCapture { bytes, truncated })
}

fn read_changed_paths(mut stdout: impl Read, args: &str) -> Result<(Vec<String>, bool), GitError> {
    let mut paths = Vec::with_capacity(MAX_CHANGED_PATHS);
    let mut path = Vec::with_capacity(256);
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = stdout
            .read(&mut chunk)
            .map_err(|error| GitError::CommandFailed {
                args: args.to_string(),
                message: format!("could not read Git stdout: {error}"),
            })?;
        if read == 0 {
            break;
        }
        for byte in &chunk[..read] {
            if paths.len() == MAX_CHANGED_PATHS {
                // The first byte after the 3,000th NUL-terminated record is
                // enough to prove GitHub's comparison window was truncated;
                // do not retain any part of path 3,001.
                return Ok((paths, true));
            }
            if *byte == 0 {
                paths.push(path_from_bytes(std::mem::take(&mut path)));
                path = Vec::with_capacity(256);
            } else {
                if path.len() == MAX_CHANGED_PATH_BYTES {
                    return Err(GitError::ChangedPathLimit {
                        args: args.to_string(),
                        max_bytes: MAX_CHANGED_PATH_BYTES,
                    });
                }
                path.push(*byte);
            }
        }
    }
    if !path.is_empty() {
        paths.push(path_from_bytes(path));
    }
    Ok((paths, false))
}

fn path_from_bytes(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(path) => path,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

fn wait_for_git(child: &mut Child, args: &[&str]) -> Result<(ExitStatus, bool), GitError> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok((status, false)),
            Ok(None) => {}
            Err(error) => {
                let mut message = format!("could not query Git process status: {error}");
                if let Err(cleanup) = terminate_child(child) {
                    message.push_str(&format!("; process cleanup also failed: {cleanup}"));
                }
                return Err(command_failed(args, message));
            }
        }
        if started.elapsed() >= Duration::from_secs(GIT_COMMAND_TIMEOUT_SECONDS) {
            let status = terminate_child(child).map_err(|error| {
                command_failed(
                    args,
                    format!("Git exceeded its deadline and could not be stopped: {error}"),
                )
            })?;
            return Ok((status, true));
        }
        std::thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

fn terminate_child(child: &mut Child) -> Result<ExitStatus, String> {
    match child.try_wait() {
        Ok(Some(status)) => return Ok(status),
        Ok(None) => {}
        Err(error) => return Err(format!("could not query process status: {error}")),
    }
    if let Err(kill_error) = child.kill() {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => return Err(format!("could not kill process: {kill_error}")),
            Err(wait_error) => {
                return Err(format!(
                    "could not kill process: {kill_error}; could not query it afterward: {wait_error}"
                ));
            }
        }
    }
    child
        .wait()
        .map_err(|error| format!("could not reap process: {error}"))
}

fn join_reader<T>(
    reader: JoinHandle<Result<T, GitError>>,
    args: &[&str],
    stream: &'static str,
) -> Result<T, GitError> {
    reader
        .join()
        .map_err(|_| command_failed(args, format!("Git {stream} reader terminated unexpectedly")))?
}

fn abort_setup<T>(
    child: &mut Child,
    args: &[&str],
    mut message: String,
    reader: Option<JoinHandle<Result<T, GitError>>>,
) -> GitError {
    if let Err(error) = terminate_child(child) {
        message.push_str(&format!("; process cleanup also failed: {error}"));
    }
    if let Some(reader) = reader {
        match reader.join() {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                message.push_str(&format!("; stdout cleanup also failed: {error}"));
            }
            Err(_) => message.push_str("; stdout reader terminated unexpectedly during cleanup"),
        }
    }
    command_failed(args, message)
}

fn command_failed(args: &[&str], message: String) -> GitError {
    GitError::CommandFailed {
        args: args.join(" "),
        message,
    }
}
