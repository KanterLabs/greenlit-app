//! Activity-aware workspace readiness: poll the container until
//! `greenlit-init` establishes isolation, surfacing its progress and failing
//! only on genuine inactivity — never on a healthy-but-long checkout copy.
//!
//! One short exec per poll reads two signals in a single round-trip: the
//! ready marker (touched by the idle command once init has exec'd) and the
//! status file init maintains (`/greenlit/status` — the format is authored
//! in `greenlit-init/src/status.rs`; the two crates deliberately do not
//! link, so this parser must stay in lockstep with that writer). Progress
//! feeds the run's [`ProgressSink`]; a `phase=failed` snapshot carries
//! init's own error for a truthful report, replacing the old fixed 60s
//! busy-wait whose only outcome was a misleading "provide sh and tail" hint.

use std::time::Duration;

use tokio::time::Instant;

use crate::engine::{ContainerEngine, ExecSpec};
use crate::executor::ExecError;
use crate::executor::context::CaptureSink;
use crate::progress::{ProgressEvent, ProgressSink, WorkspaceProgress};

/// In-container path of init's status file. Mirrors
/// `greenlit-init/src/status.rs` (see module docs).
const STATUS_PATH: &str = "/greenlit/status";

/// In-container path of the readiness marker the idle command touches.
pub(crate) const READY_MARKER: &str = "/greenlit/ready";

/// The line the probe prints when [`READY_MARKER`] exists.
const READY_TOKEN: &str = "GREENLIT_READY";

/// Cadence and deadlines for the workspace-readiness poll.
///
/// There is deliberately no absolute wall-clock cap: copy progress is
/// monotone over a finite tree, so continued activity implies eventual
/// termination, and a wall cap would re-introduce the false "never became
/// ready" failure on large checkouts that this design removes.
#[derive(Debug, Clone, Copy)]
pub struct ReadinessConfig {
    /// Delay between probe execs.
    pub poll_interval: Duration,
    /// How long to wait for *any* signal (marker or status file) before
    /// concluding init never started.
    pub first_signal_timeout: Duration,
    /// How long the status may stay unchanged before the setup counts as
    /// stalled.
    pub inactivity_timeout: Duration,
}

impl Default for ReadinessConfig {
    fn default() -> Self {
        ReadinessConfig {
            poll_interval: Duration::from_millis(250),
            first_signal_timeout: Duration::from_secs(10),
            inactivity_timeout: Duration::from_secs(30),
        }
    }
}

/// One parsed status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct StatusSnapshot {
    phase: String,
    strategy: Option<String>,
    fell_back: bool,
    reason: Option<String>,
    files: u64,
    bytes: u64,
    detail: String,
}

/// Parses the status file's `key=value` first line plus detail lines.
fn parse_status(text: &str) -> Option<StatusSnapshot> {
    let mut lines = text.lines();
    let first = lines.next()?;
    let mut snapshot = StatusSnapshot {
        detail: lines.collect::<Vec<_>>().join("\n"),
        ..StatusSnapshot::default()
    };
    let mut versioned = false;
    for token in first.split_whitespace() {
        let (key, value) = token.split_once('=')?;
        match key {
            "v" => versioned = true,
            "phase" => snapshot.phase = value.to_string(),
            "strategy" => snapshot.strategy = Some(value.to_string()),
            "fell_back" => snapshot.fell_back = value == "1",
            "reason" => snapshot.reason = Some(value.to_string()),
            "files" => snapshot.files = value.parse().ok()?,
            "bytes" => snapshot.bytes = value.parse().ok()?,
            _ => {}
        }
    }
    (versioned && !snapshot.phase.is_empty()).then_some(snapshot)
}

/// Polls until init signals readiness, emitting workspace progress.
///
/// # Errors
///
/// Returns [`ExecError::Infrastructure`] naming what was actually observed:
/// init's own failure detail, a stalled copy (with its counts and the
/// clean-clone fix), a container that never reported at all, or an
/// unprobeable container.
pub(crate) async fn wait_for_ready(
    engine: &dyn ContainerEngine,
    container: &str,
    config: &ReadinessConfig,
    progress: &mut (dyn ProgressSink + Send),
) -> Result<(), ExecError> {
    let program =
        format!("[ -e {READY_MARKER} ] && echo {READY_TOKEN}; cat {STATUS_PATH} 2>/dev/null");
    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), program],
        env: Vec::new(),
        working_dir: None,
    };

    let started = Instant::now();
    let mut last_change = started;
    let mut last_status_text: Option<String> = None;
    let mut last_snapshot: Option<StatusSnapshot> = None;
    let mut fell_back_reported = false;
    let mut probed_ok = false;

    loop {
        let mut sink = CaptureSink::default();
        let output = match engine.exec(container, &spec, &mut sink).await {
            Ok(output) => output,
            Err(error) => return Err(probe_failure(engine, container, error, probed_ok).await),
        };
        probed_ok = true;
        let text = sink.text();
        let ready = text.lines().any(|line| line.trim() == READY_TOKEN);
        let status_text = match text.split_once('\n') {
            Some((first, rest)) if first.trim() == READY_TOKEN => rest.to_string(),
            _ => text.clone(),
        };

        if !status_text.trim().is_empty() && Some(&status_text) != last_status_text.as_ref() {
            last_change = Instant::now();
            if let Some(snapshot) = parse_status(&status_text) {
                if snapshot.fell_back && !fell_back_reported {
                    fell_back_reported = true;
                    progress.on_progress(ProgressEvent::Workspace(WorkspaceProgress::FellBack {
                        reason: snapshot.reason.clone().unwrap_or_else(|| "unknown".into()),
                    }));
                }
                match snapshot.phase.as_str() {
                    "copy" => {
                        progress.on_progress(ProgressEvent::Workspace(
                            WorkspaceProgress::Copying {
                                files: snapshot.files,
                                bytes: snapshot.bytes,
                            },
                        ));
                    }
                    "failed" => return Err(init_failure(&snapshot)),
                    _ => {}
                }
                last_snapshot = Some(snapshot);
            }
            last_status_text = Some(status_text);
        }

        if ready {
            let strategy = last_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.strategy.clone())
                .unwrap_or_else(|| "overlay".to_string());
            progress.on_progress(ProgressEvent::Workspace(WorkspaceProgress::Ready {
                strategy,
            }));
            // The exit code only reflects the marker test; readiness is the
            // marker itself.
            let _ = output;
            return Ok(());
        }

        if last_status_text.is_none() && started.elapsed() > config.first_signal_timeout {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "the job container never reported isolation status (greenlit-init did not \
                     start within {}s)",
                    config.first_signal_timeout.as_secs()
                ),
                fix: "ensure the runner image provides a POSIX `sh`; for a `container:` job, \
                      check the image can execute the mounted /greenlit/bin/greenlit-init"
                    .to_string(),
            });
        }
        if last_status_text.is_some() && last_change.elapsed() > config.inactivity_timeout {
            return Err(stall_failure(last_snapshot.as_ref(), config));
        }

        tokio::time::sleep(config.poll_interval).await;
    }
}

/// The error for a status file frozen past the inactivity deadline.
fn stall_failure(snapshot: Option<&StatusSnapshot>, config: &ReadinessConfig) -> ExecError {
    let stalled_for = config.inactivity_timeout.as_secs();
    match snapshot {
        Some(snapshot) if snapshot.phase == "copy" => ExecError::Infrastructure {
            message: format!(
                "workspace setup stalled while copying the checkout ({} files, {} bytes copied; \
                 no progress for {stalled_for}s)",
                snapshot.files, snapshot.bytes
            ),
            fix: "the checkout may contain large build artifacts (e.g. target/); remove them or \
                  run from a clean clone"
                .to_string(),
        },
        Some(snapshot) => ExecError::Infrastructure {
            message: format!(
                "workspace setup stalled during the {} phase (no progress for {stalled_for}s)",
                snapshot.phase
            ),
            fix: "retry the run; if it persists, run with `--isolation copy-in` to skip the \
                  overlay path"
                .to_string(),
        },
        None => ExecError::Infrastructure {
            message: format!(
                "workspace setup stalled before reporting a phase (no progress for {stalled_for}s)"
            ),
            fix: "retry the run".to_string(),
        },
    }
}

/// The error carrying init's own `phase=failed` report.
fn init_failure(snapshot: &StatusSnapshot) -> ExecError {
    let detail = if snapshot.detail.trim().is_empty() {
        "no detail reported".to_string()
    } else {
        snapshot.detail.trim().to_string()
    };
    ExecError::Infrastructure {
        message: format!("workspace isolation failed inside the container: {detail}"),
        fix: "fix the isolation failure reported above, then retry".to_string(),
    }
}

/// Classifies a probe exec failure. Before any successful probe the likely
/// cause is a missing `sh`; afterwards the container has probably died, so
/// recover init's last status from the stopped container if possible.
async fn probe_failure(
    engine: &dyn ContainerEngine,
    container: &str,
    error: crate::error::RuntimeError,
    probed_ok: bool,
) -> ExecError {
    if !probed_ok {
        return ExecError::Infrastructure {
            message: format!("could not probe the job container: {error}"),
            fix: "ensure the runner image provides a POSIX `sh`".to_string(),
        };
    }
    // `export_path` works on stopped containers; init writes `phase=failed`
    // before exiting, so a dead container usually still carries the story.
    if let Ok(tar_bytes) = engine.export_path(container, STATUS_PATH).await
        && let Some(text) = read_single_tar_entry(&tar_bytes)
        && let Some(snapshot) = parse_status(&text)
        && snapshot.phase == "failed"
    {
        return init_failure(&snapshot);
    }
    ExecError::Engine(error)
}

/// Extracts the first regular file's contents from a tar export.
fn read_single_tar_entry(tar_bytes: &[u8]) -> Option<String> {
    use std::io::Read;
    let mut archive = tar::Archive::new(tar_bytes);
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        if entry.header().entry_type().is_file() {
            let mut contents = String::new();
            entry.read_to_string(&mut contents).ok()?;
            return Some(contents);
        }
    }
    None
}
