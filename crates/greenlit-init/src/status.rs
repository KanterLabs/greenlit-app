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
//! `files=N`, `bytes=N` — and any following lines are free-form human detail
//! (the failure message). The host escapes the detail before rendering it.
//! The runtime crate parses this format; the two crates deliberately do not
//! link (see [`crate::strategy::FALLBACK_MARKER`]'s cross-crate precedent),
//! so changes here must stay in lockstep with
//! `greenlit-runtime/src/executor/readiness.rs`.

use std::fs;
use std::path::{Path, PathBuf};

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
    pub(crate) fn copy(&self, files: u64, bytes: u64, fell_back: Option<&str>) {
        let mut line = "v=1 phase=copy strategy=copy-in".to_string();
        if let Some(reason) = fell_back {
            line.push_str(&format!(" fell_back=1 reason={reason}"));
        }
        line.push_str(&format!(" files={files} bytes={bytes}"));
        self.write(line, None);
    }

    /// Isolation is established; the job command is about to exec.
    pub(crate) fn done(&self, strategy: &str, fell_back: Option<&str>) {
        let mut line = format!("v=1 phase=done strategy={strategy}");
        if let Some(reason) = fell_back {
            line.push_str(&format!(" fell_back=1 reason={reason}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn read(writer: &StatusWriter) -> String {
        fs::read_to_string(&writer.path).expect("status file")
    }

    #[test]
    fn each_phase_writes_its_exact_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = StatusWriter::in_dir(dir.path());

        writer.start();
        assert_eq!(read(&writer), "v=1 phase=start\n");

        writer.overlay();
        assert_eq!(read(&writer), "v=1 phase=overlay\n");

        writer.copy(0, 0, None);
        assert_eq!(
            read(&writer),
            "v=1 phase=copy strategy=copy-in files=0 bytes=0\n"
        );

        writer.copy(1234, 987654, Some("EPERM"));
        assert_eq!(
            read(&writer),
            "v=1 phase=copy strategy=copy-in fell_back=1 reason=EPERM files=1234 bytes=987654\n"
        );

        writer.done("overlay", None);
        assert_eq!(read(&writer), "v=1 phase=done strategy=overlay\n");

        writer.done("copy-in", Some("EINVAL"));
        assert_eq!(
            read(&writer),
            "v=1 phase=done strategy=copy-in fell_back=1 reason=EINVAL\n"
        );
    }

    #[test]
    fn failure_carries_the_detail_lines_after_the_status_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = StatusWriter::in_dir(dir.path());
        writer.failed("could not copy /x: permission denied");
        assert_eq!(
            read(&writer),
            "v=1 phase=failed\ncould not copy /x: permission denied\n"
        );
    }

    #[test]
    fn a_write_into_a_missing_directory_is_silently_advisory() {
        let writer = StatusWriter::in_dir(Path::new("/nonexistent-greenlit-status-dir"));
        // Must not panic or error — status is best-effort by invariant.
        writer.start();
        writer.failed("detail");
    }

    #[test]
    fn writes_replace_atomically_leaving_no_tmp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = StatusWriter::in_dir(dir.path());
        writer.start();
        writer.overlay();
        assert!(!writer.tmp.exists(), "the tmp file is always renamed away");
    }
}
