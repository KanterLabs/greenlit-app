//! The live step-output sink: assembles lines from raw daemon chunks, folds
//! `::group::` blocks, routes annotations, and applies secret masking to every
//! line before it reaches the terminal.
//!
//! `PHASE-2-execution.md` ("Log commands", "Output and metrics") and the
//! `greenlit-v0-spec.md` security model: `::add-mask::` takes effect
//! immediately, so masking is applied on the live stream, chunk by chunk, and a
//! registered value never reaches the log. The workflow-command grammar and the
//! [`Masker`] itself are the engine's (`greenlit_engine::execution`); this sink
//! is the runtime glue that drives them over a real exec's byte stream.

use std::io::Write;

use greenlit_engine::execution::log_commands::{Annotation, LogLine, Masker, parse_line};

use crate::engine::ExecOutputSink;

/// A [`ExecOutputSink`] that renders a step's output live, honoring workflow
/// log commands and masking. Borrows the shared run [`Masker`] (so
/// `::add-mask::` persists) and the terminal writer.
pub struct StepLogSink<'a> {
    out: &'a mut (dyn Write + Send),
    masker: &'a mut Masker,
    stdout_line: Vec<u8>,
    stderr_line: Vec<u8>,
    group_depth: usize,
    /// Counts of annotations seen, for the caller's summary.
    pub errors: usize,
    /// Warning annotations seen.
    pub warnings: usize,
    /// Any write to the terminal failed; the run continues but the log is
    /// incomplete. Surfaced so the caller can note it.
    pub write_failed: bool,
}

impl<'a> StepLogSink<'a> {
    /// Build a sink writing to `out`, masking with the shared `masker`.
    pub fn new(out: &'a mut (dyn Write + Send), masker: &'a mut Masker) -> Self {
        StepLogSink {
            out,
            masker,
            stdout_line: Vec::new(),
            stderr_line: Vec::new(),
            group_depth: 0,
            errors: 0,
            warnings: 0,
            write_failed: false,
        }
    }

    /// Flush any buffered partial lines (output that lacked a trailing newline).
    /// Called once the exec has finished streaming.
    pub fn finish(&mut self) {
        if !self.stdout_line.is_empty() {
            let line = std::mem::take(&mut self.stdout_line);
            self.emit(&line);
        }
        if !self.stderr_line.is_empty() {
            let line = std::mem::take(&mut self.stderr_line);
            self.emit(&line);
        }
    }

    /// Process one complete raw line (without its terminator).
    fn emit(&mut self, raw: &[u8]) {
        // Non-UTF-8 output is rendered lossily; workflow commands are ASCII, so
        // a lossy view never misclassifies one.
        let text = String::from_utf8_lossy(raw);
        match parse_line(&text) {
            LogLine::AddMask(value) => {
                // The value itself is never printed — registering it is the
                // whole point. Later lines that echo it are redacted.
                self.masker.add(&value);
            }
            LogLine::StartGroup(title) => {
                let title = self.masker.apply(&title);
                self.line(&format!("\u{25be} {title}"));
                self.group_depth = self.group_depth.saturating_add(1);
            }
            LogLine::EndGroup => {
                self.group_depth = self.group_depth.saturating_sub(1);
            }
            LogLine::Error(annotation) => {
                self.errors = self.errors.saturating_add(1);
                self.annotation("error", &annotation);
            }
            LogLine::Warning(annotation) => {
                self.warnings = self.warnings.saturating_add(1);
                self.annotation("warning", &annotation);
            }
            LogLine::Notice(annotation) => self.annotation("notice", &annotation),
            // Debug lines mirror GitHub: shown only under debug logging, which
            // v0 does not enable, so they are dropped from the live log.
            LogLine::Debug(_) => {}
            LogLine::Output(text) => {
                let redacted = self.masker.apply(&text);
                self.line(&redacted);
            }
        }
    }

    /// Render one annotation line with its severity prefix.
    fn annotation(&mut self, kind: &str, annotation: &Annotation) {
        let message = self.masker.apply(&annotation.message);
        self.line(&format!("[{kind}] {message}"));
    }

    /// Write one already-processed line to the terminal, indented by group
    /// depth.
    fn line(&mut self, text: &str) {
        let indent = "  ".repeat(self.group_depth);
        if writeln!(self.out, "{indent}{text}").is_err() {
            self.write_failed = true;
        }
    }
}

impl ExecOutputSink for StepLogSink<'_> {
    fn on_stdout(&mut self, chunk: &[u8]) {
        // Take ownership of the buffer to satisfy the borrow checker while
        // `emit` borrows `self`.
        let mut buffer = std::mem::take(&mut self.stdout_line);
        buffer.extend_from_slice(chunk);
        self.stdout_line = buffer;
        drain(self, false);
    }

    fn on_stderr(&mut self, chunk: &[u8]) {
        let mut buffer = std::mem::take(&mut self.stderr_line);
        buffer.extend_from_slice(chunk);
        self.stderr_line = buffer;
        drain(self, true);
    }
}

/// Emit every complete line currently buffered on the given stream.
fn drain(sink: &mut StepLogSink<'_>, is_stderr: bool) {
    loop {
        let position = {
            let buffer = if is_stderr {
                &sink.stderr_line
            } else {
                &sink.stdout_line
            };
            buffer.iter().position(|&b| b == b'\n')
        };
        let Some(newline) = position else { break };
        let mut line: Vec<u8> = {
            let buffer = if is_stderr {
                &mut sink.stderr_line
            } else {
                &mut sink.stdout_line
            };
            buffer.drain(..=newline).collect()
        };
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        sink.emit(&line);
    }
}
