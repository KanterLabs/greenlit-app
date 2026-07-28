//! Machine-readable isolation status under `/greenlit/status`.
//!
//! The host cannot see this process's stderr (it goes to the container log,
//! which `litci` does not read), so isolation progress is reported through a
//! small status file the host polls while it waits for readiness. Writes are
//! atomic (write `status.tmp`, then rename) so the host never reads a torn
//! line, and **best-effort** — status is advisory; a status write failure
//! must never fail isolation itself, so every write discards its result.
//!
//! Format: line 1 is `key=value` tokens — `v=1 phase=<start|overlay|copy|
//! done|failed>` plus optional `strategy=`, `fell_back=`, `reason=<ERRNO>`,
//! `files=N`, `bytes=N`, `reflink_files=N`, and `bounded_stream_files=N` — and
//! any following lines are free-form human detail (the failure message). The
//! host escapes the detail before rendering it.
//! The runtime crate parses this format; the two crates deliberately do not
//! link (see [`crate::strategy::FALLBACK_MARKER`]'s cross-crate precedent),
//! so changes here must stay in lockstep with
//! `greenlit-runtime/src/executor/readiness.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::copy_in::CopyStats;

/// Where the status file lives inside the container. `/greenlit` exists in
/// every job container (the repo bind and ready marker already live there).
const STATUS_DIR: &str = "/greenlit";

/// Writes isolation-status snapshots for the host to poll.
pub(crate) struct StatusWriter {
    path: PathBuf,
    tmp: PathBuf,
}

impl StatusWriter {
    /// The production writer at [`STATUS_DIR`].
    pub(crate) fn new() -> Self {
        Self::in_dir(Path::new(STATUS_DIR))
    }

    /// A writer rooted at `dir` (tests use a temp dir).
    pub(crate) fn in_dir(dir: &Path) -> Self {
        StatusWriter {
            path: dir.join("status"),
            tmp: dir.join("status.tmp"),
        }
    }

    /// Isolation setup has begun.
    pub(crate) fn start(&self) {
        self.write("v=1 phase=start".to_string(), None);
    }

    /// The overlay mount is about to be attempted.
    pub(crate) fn overlay(&self) {
        self.write("v=1 phase=overlay".to_string(), None);
    }

    /// The checkout copy is running. `fell_back` carries the mount errno
    /// name when copy-in is the fallback rather than the requested strategy.
    pub(crate) fn copy(&self, stats: CopyStats, fell_back: Option<&str>) {
        let mut line = "v=1 phase=copy strategy=copy-in".to_string();
        if let Some(reason) = fell_back {
            line.push_str(&format!(" fell_back=1 reason={reason}"));
        }
        append_copy_stats(&mut line, stats);
        self.write(line, None);
    }

    /// Isolation is established; the job command is about to exec.
    pub(crate) fn done(
        &self,
        strategy: &str,
        fell_back: Option<&str>,
        copy_stats: Option<CopyStats>,
    ) {
        let mut line = format!("v=1 phase=done strategy={strategy}");
        if let Some(reason) = fell_back {
            line.push_str(&format!(" fell_back=1 reason={reason}"));
        }
        if let Some(stats) = copy_stats {
            append_copy_stats(&mut line, stats);
        }
        self.write(line, None);
    }

    /// Isolation (or the final exec) failed; `detail` is the error rendering
    /// the host should surface.
    pub(crate) fn failed(&self, detail: &str) {
        self.write("v=1 phase=failed".to_string(), Some(detail));
    }

    fn write(&self, line: String, detail: Option<&str>) {
        let mut contents = line;
        if let Some(detail) = detail {
            contents.push('\n');
            contents.push_str(detail);
        }
        contents.push('\n');
        // Advisory only — never let a status write failure fail isolation.
        let _ = fs::write(&self.tmp, contents);
        let _ = fs::rename(&self.tmp, &self.path);
    }
}

fn append_copy_stats(line: &mut String, stats: CopyStats) {
    line.push_str(&format!(
        " files={} bytes={} reflink_files={} bounded_stream_files={}",
        stats.files, stats.bytes, stats.reflink_files, stats.bounded_stream_files
    ));
}
