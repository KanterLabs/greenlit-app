//! Shared bounded, deadline-enforced execution for the `git` subprocesses
//! this crate spawns against a *remote* — `git ls-remote`
//! ([`crate::resolve::git_ls_remote`]) and `git clone`/`fetch`
//! ([`crate::store::git_clone`]).
//!
//! This mirrors `crates/greenlit-engine/src/git/process.rs`'s conventions —
//! a fixed wall-clock deadline that kills the child, thread-drained bounded
//! stdout/stderr so a hung or chatty remote cannot block or exhaust memory —
//! adapted for a network operation rather than a local read-only query:
//! there is no `-C <repo_root>` (no local repository is involved in
//! resolving or fetching a *different* GitHub repository), and the deadline
//! is sized for network latency rather than a local disk read.

use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// `git ls-remote` output for the handful of ref patterns this crate
/// queries is a few lines at most; this generously bounds it against a
/// hostile or misbehaving remote rather than reflecting any real expected
/// size.
pub(crate) const MAX_GIT_STDOUT_BYTES: usize = 256 * 1024;
/// Same reasoning as `MAX_GIT_STDOUT_BYTES`, sized down: diagnostics are
/// short.
pub(crate) const MAX_GIT_STDERR_BYTES: usize = 64 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A finished (or timed-out) git subprocess's bounded output.
pub(crate) struct GitProcessOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stderr_truncated: bool,
}

/// A failure spawning or waiting on a git subprocess, independent of
/// whatever the remote itself said (a non-zero exit status is carried in
/// [`GitProcessOutput::status`] instead — only process-level failures are
/// errors here).
#[derive(Debug, Clone)]
pub(crate) enum GitProcessError {
    /// The `git` binary could not even be spawned.
    Spawn { args: String, message: String },
    /// Reading its output, or waiting on it, failed at the OS level.
    Io { args: String, message: String },
    /// It did not exit within `deadline` and was killed.
    TimedOut { seconds: u64 },
}

/// Runs `git <args>` (optionally in `cwd`), capturing bounded stdout/stderr
/// and enforcing `deadline`.
///
/// A non-zero exit is *not* an error here — callers inspect
/// [`GitProcessOutput::status`] and `stderr` themselves, since "ref/repo not
/// found" and "process genuinely failed" need different treatment per
/// caller.
pub(crate) fn run_git(
    cwd: Option<&Path>,
    args: &[&str],
    deadline: Duration,
) -> Result<GitProcessOutput, GitProcessError> {
    let args_joined = args.join(" ");
    let mut command = Command::new("git");
    command
        .args(args)
        // Never prompt for credentials: a private/rate-limited remote must
        // fail fast rather than hang waiting for input that can never
        // arrive from an unattended run.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let mut child = command.spawn().map_err(|error| GitProcessError::Spawn {
        args: args_joined.clone(),
        message: format!("could not start Git: {error}"),
    })?;

    let stdout = child.stdout.take().ok_or_else(|| GitProcessError::Io {
        args: args_joined.clone(),
        message: "could not capture Git stdout".to_owned(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| GitProcessError::Io {
        args: args_joined.clone(),
        message: "could not capture Git stderr".to_owned(),
    })?;

    let stdout_reader = spawn_reader(stdout, MAX_GIT_STDOUT_BYTES, &args_joined, "stdout")
        .map_err(|message| GitProcessError::Io {
            args: args_joined.clone(),
            message,
        })?;
    let stderr_reader = spawn_reader(stderr, MAX_GIT_STDERR_BYTES, &args_joined, "stderr")
        .map_err(|message| GitProcessError::Io {
            args: args_joined.clone(),
            message,
        })?;

    let (status, timed_out) = wait_with_deadline(&mut child, deadline, &args_joined)?;
    let stdout_result = join_reader(stdout_reader, &args_joined)?;
    let stderr_result = join_reader(stderr_reader, &args_joined)?;

    if timed_out {
        return Err(GitProcessError::TimedOut {
            seconds: deadline.as_secs(),
        });
    }

    Ok(GitProcessOutput {
        status,
        stdout: stdout_result.bytes,
        stdout_truncated: stdout_result.truncated,
        stderr: stderr_result.bytes,
        stderr_truncated: stderr_result.truncated,
    })
}

struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

trait BoundedReadable: Read + Send + 'static {}
impl BoundedReadable for ChildStdout {}
impl BoundedReadable for ChildStderr {}

fn spawn_reader(
    reader: impl BoundedReadable,
    max_bytes: usize,
    args: &str,
    stream: &'static str,
) -> Result<JoinHandle<Result<BoundedCapture, String>>, String> {
    let args = args.to_owned();
    std::thread::Builder::new()
        .name(format!("litci-actions-git-{stream}"))
        .spawn(move || read_bounded(reader, max_bytes, &args, stream))
        .map_err(|error| format!("could not start Git {stream} reader: {error}"))
}

fn read_bounded(
    mut reader: impl Read,
    max_bytes: usize,
    args: &str,
    stream: &'static str,
) -> Result<BoundedCapture, String> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut truncated = false;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|error| format!("could not read Git {stream} for '{args}': {error}"))?;
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

fn join_reader(
    reader: JoinHandle<Result<BoundedCapture, String>>,
    args: &str,
) -> Result<BoundedCapture, GitProcessError> {
    reader
        .join()
        .map_err(|_| GitProcessError::Io {
            args: args.to_owned(),
            message: "Git output reader terminated unexpectedly".to_owned(),
        })?
        .map_err(|message| GitProcessError::Io {
            args: args.to_owned(),
            message,
        })
}

fn wait_with_deadline(
    child: &mut Child,
    deadline: Duration,
    args: &str,
) -> Result<(ExitStatus, bool), GitProcessError> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok((status, false)),
            Ok(None) => {}
            Err(error) => {
                let mut message = format!("could not query Git process status: {error}");
                if let Err(cleanup) = terminate(child) {
                    message.push_str(&format!("; process cleanup also failed: {cleanup}"));
                }
                return Err(GitProcessError::Io {
                    args: args.to_owned(),
                    message,
                });
            }
        }
        if started.elapsed() >= deadline {
            let status = terminate(child).map_err(|error| GitProcessError::Io {
                args: args.to_owned(),
                message: format!("Git exceeded its deadline and could not be stopped: {error}"),
            })?;
            return Ok((status, true));
        }
        std::thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

fn terminate(child: &mut Child) -> Result<ExitStatus, String> {
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

/// Renders `stderr` for embedding in a caller's error message: UTF-8 lossy,
/// trimmed, with a truncation note appended when the bound above was hit.
pub(crate) fn diagnostic(stderr: &[u8], truncated: bool) -> String {
    let mut text = String::from_utf8_lossy(stderr).trim().to_string();
    if truncated {
        text.push_str(&format!(
            " [diagnostic truncated at {MAX_GIT_STDERR_BYTES} bytes]"
        ));
    }
    text
}
