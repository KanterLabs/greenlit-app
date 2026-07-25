//! Renders run-phase [`ProgressEvent`]s on stderr.
//!
//! stdout is the machine-parseable run log, so phase progress lives on
//! stderr, which is otherwise free while a run executes (the timing render
//! and write-back prompts are post-run). On a terminal the renderer keeps
//! one transient status line, rewritten in place with `\r` and erased when a
//! phase ends so it never collides with the stdout job/step lines that
//! follow. Off a terminal it prints phase start/end lines only — never
//! per-chunk updates — so CI logs stay small.
//!
//! Every dynamic string in an event (image names, daemon build lines, init
//! status text) is untrusted display text and goes through
//! [`super::terminal::inline_escape`] before it reaches the stream.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use greenlit_runtime::{ProgressEvent, ProgressSink, WorkspaceProgress};

use super::terminal::inline_escape;

/// Minimum interval between transient-line redraws for high-rate events
/// (pull chunks, build lines, copy ticks). Phase transitions always draw.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// The stderr progress renderer used by `litci run`.
pub(crate) fn renderer_for_stderr() -> ProgressRenderer<std::io::Stderr> {
    let tty = std::io::stderr().is_terminal();
    ProgressRenderer::new(std::io::stderr(), tty)
}

/// Draws [`ProgressEvent`]s onto one writer; `tty` selects the transient
/// single-line mode versus plain phase lines.
pub(crate) struct ProgressRenderer<W: Write> {
    out: W,
    tty: bool,
    /// Display width of the transient line currently on screen (0 = none).
    shown_width: usize,
    /// When the transient line was last redrawn.
    last_draw: Option<Instant>,
}

impl<W: Write> ProgressRenderer<W> {
    pub(crate) fn new(out: W, tty: bool) -> Self {
        ProgressRenderer {
            out,
            tty,
            shown_width: 0,
            last_draw: None,
        }
    }

    /// Rewrites the transient line in place (terminal mode only).
    fn show_transient(&mut self, text: &str, force: bool) {
        if !self.tty {
            return;
        }
        if !force
            && let Some(last) = self.last_draw
            && last.elapsed() < REDRAW_INTERVAL
        {
            return;
        }
        let width = text.chars().count();
        // Pad over any longer prior line; the padding itself is blank, so
        // only the new text width needs erasing next time.
        let padding = self.shown_width.saturating_sub(width);
        let _ = write!(self.out, "\r{text}{}", " ".repeat(padding));
        let _ = self.out.flush();
        self.shown_width = width;
        self.last_draw = Some(Instant::now());
    }

    /// Erases the transient line (terminal mode only).
    fn clear_transient(&mut self) {
        if self.tty && self.shown_width > 0 {
            let _ = write!(self.out, "\r{}\r", " ".repeat(self.shown_width));
            let _ = self.out.flush();
        }
        self.shown_width = 0;
        self.last_draw = None;
    }

    /// Writes a permanent full line, first erasing any transient line.
    fn permanent(&mut self, text: &str) {
        self.clear_transient();
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }

    /// Writes a phase start/end line in non-terminal mode only.
    fn phase_line(&mut self, text: &str) {
        if !self.tty {
            let _ = writeln!(self.out, "{text}");
            let _ = self.out.flush();
        }
    }
}

impl<W: Write + Send> ProgressSink for ProgressRenderer<W> {
    fn on_progress(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::PullStarted { image } => {
                let image = inline_escape(&image);
                self.phase_line(&format!("image-ensure: pulling {image}"));
                self.show_transient(&format!("image-ensure: pulling {image}"), true);
            }
            ProgressEvent::PullProgress {
                current_bytes,
                total_bytes,
            } => {
                let progress = match total_bytes {
                    Some(total) => format!("{} / {}", fmt_bytes(current_bytes), fmt_bytes(total)),
                    None => fmt_bytes(current_bytes),
                };
                self.show_transient(&format!("image-ensure: pulling ({progress})"), false);
            }
            ProgressEvent::PullFinished {
                image,
                downloaded_bytes,
            } => {
                let image = inline_escape(&image);
                if downloaded_bytes == 0 {
                    self.phase_line(&format!("image-ensure: cache hit {image} (0 B downloaded)"));
                } else {
                    self.phase_line(&format!(
                        "image-ensure: downloaded {} for {image}",
                        fmt_bytes(downloaded_bytes)
                    ));
                }
                self.clear_transient();
            }
            ProgressEvent::ContentResolved {
                item,
                identity,
                cache_hit,
            } => {
                let source = if cache_hit { "CAS hit" } else { "verified now" };
                self.phase_line(&format!(
                    "image-resolve: {} -> {} ({source})",
                    inline_escape(&item),
                    inline_escape(&identity)
                ));
            }
            ProgressEvent::BuildStarted { tag } => {
                let tag = inline_escape(&tag);
                self.phase_line(&format!("image-ensure: building {tag}"));
                self.show_transient(&format!("image-ensure: building {tag}"), true);
            }
            ProgressEvent::BuildLine { line } => {
                self.show_transient(
                    &format!("image-ensure: building — {}", inline_escape(&line)),
                    false,
                );
            }
            ProgressEvent::BuildFinished { tag } => {
                self.phase_line(&format!("image-ensure: built {}", inline_escape(&tag)));
                self.clear_transient();
            }
            // Sub-second phase: worth a transient heartbeat on a terminal,
            // noise in a CI log.
            ProgressEvent::BootStarted => {
                self.show_transient("container-boot: starting job container", true);
            }
            ProgressEvent::BootFinished => self.clear_transient(),
            ProgressEvent::Workspace(workspace) => match workspace {
                // Must-retain information in both modes: the run is not using
                // the isolation strategy the user may expect, and `--write-back`
                // would be unavailable.
                WorkspaceProgress::FellBack { reason } => {
                    self.permanent(&format!(
                        "overlay-setup: unprivileged overlayfs unavailable ({}); copying the \
                         checkout into the workspace instead",
                        inline_escape(&reason)
                    ));
                }
                WorkspaceProgress::Copying { files, bytes } => {
                    self.show_transient(
                        &format!(
                            "overlay-setup: copying checkout ({} files, {})",
                            fmt_count(files),
                            fmt_bytes(bytes)
                        ),
                        false,
                    );
                }
                WorkspaceProgress::Ready { strategy } => {
                    self.phase_line(&format!(
                        "overlay-setup: workspace ready ({})",
                        inline_escape(&strategy)
                    ));
                    self.clear_transient();
                }
                _ => {}
            },
            ProgressEvent::ActionRuntimeEnsureStarted => {
                self.show_transient("action-runtime-ensure: preparing pinned Node runtime", true);
            }
            ProgressEvent::ActionRuntimeEnsureFinished => {
                self.phase_line("action-runtime-ensure: pinned Node runtime ready");
                self.clear_transient();
            }
            _ => {}
        }
    }
}

/// Bytes as a short human figure (`512 B`, `3.4 MiB`, `1.2 GiB`).
fn fmt_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let bytes_f = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes_f < KIB * KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else if bytes_f < KIB * KIB * KIB {
        format!("{:.1} MiB", bytes_f / (KIB * KIB))
    } else {
        format!("{:.1} GiB", bytes_f / (KIB * KIB * KIB))
    }
}

/// A count with thousands separators (`12,345`).
fn fmt_count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(tty: bool, events: Vec<ProgressEvent>) -> String {
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut renderer = ProgressRenderer::new(&mut buffer, tty);
            for event in events {
                renderer.on_progress(event);
            }
        }
        String::from_utf8(buffer).expect("renderer output is UTF-8")
    }

    fn pull_script() -> Vec<ProgressEvent> {
        vec![
            ProgressEvent::PullStarted {
                image: "rust:1.96.0".to_string(),
            },
            ProgressEvent::PullProgress {
                current_bytes: 10 * 1024 * 1024,
                total_bytes: Some(100 * 1024 * 1024),
            },
            ProgressEvent::PullFinished {
                image: "rust:1.96.0".to_string(),
                downloaded_bytes: 100 * 1024 * 1024,
            },
            ProgressEvent::BootStarted,
            ProgressEvent::BootFinished,
            ProgressEvent::Workspace(WorkspaceProgress::Copying {
                files: 12345,
                bytes: 2 * 1024 * 1024 * 1024,
            }),
            ProgressEvent::Workspace(WorkspaceProgress::Ready {
                strategy: "copy-in".to_string(),
            }),
        ]
    }

    #[test]
    fn non_tty_prints_phase_lines_only_and_never_rewrites() {
        let output = drive(false, pull_script());
        assert!(output.contains("image-ensure: pulling rust:1.96.0\n"));
        assert!(output.contains("image-ensure: downloaded 100.0 MiB for rust:1.96.0\n"));
        assert!(output.contains("overlay-setup: workspace ready (copy-in)\n"));
        assert!(!output.contains('\r'), "no in-place rewrites off a tty");
        assert!(
            !output.contains("MiB /"),
            "per-chunk pull progress is dropped off a tty: {output}"
        );
        assert!(
            !output.contains("container-boot"),
            "the sub-second boot phase stays silent off a tty: {output}"
        );
        assert!(
            !output.contains("copying checkout"),
            "copy ticks are transient-only: {output}"
        );
    }

    #[test]
    fn tty_shorter_redraw_erases_the_longer_prior_line() {
        let long = "overlay-setup: copying checkout (12,345 files, 2.0 GiB)";
        let output = drive(
            true,
            vec![
                ProgressEvent::Workspace(WorkspaceProgress::Copying {
                    files: 12345,
                    bytes: 2 * 1024 * 1024 * 1024,
                }),
                ProgressEvent::Workspace(WorkspaceProgress::Ready {
                    strategy: "copy-in".to_string(),
                }),
            ],
        );
        assert!(output.contains(&format!("\r{long}")), "{output:?}");
        // The final clear pads the full width of the longest shown line and
        // returns the cursor, leaving nothing on screen.
        let clear = format!("\r{}\r", " ".repeat(long.chars().count()));
        assert!(output.ends_with(&clear), "{output:?}");
    }

    #[test]
    fn the_fallback_notice_is_a_permanent_line_in_both_modes() {
        for tty in [false, true] {
            let output = drive(
                tty,
                vec![ProgressEvent::Workspace(WorkspaceProgress::FellBack {
                    reason: "EPERM".to_string(),
                })],
            );
            assert!(
                output.contains("unprivileged overlayfs unavailable (EPERM)"),
                "tty={tty}: {output:?}"
            );
            assert!(output.contains('\n'), "tty={tty}: permanent lines newline");
        }
    }

    #[test]
    fn a_build_line_with_an_escape_byte_never_reaches_output_raw() {
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut renderer = ProgressRenderer::new(&mut buffer, true);
            renderer.on_progress(ProgressEvent::BuildStarted {
                tag: "greenlit/base:x".to_string(),
            });
            // Out-wait the redraw throttle so the build line actually draws.
            std::thread::sleep(REDRAW_INTERVAL + Duration::from_millis(20));
            renderer.on_progress(ProgressEvent::BuildLine {
                line: "step \u{1b}[2J evil".to_string(),
            });
        }
        let output = String::from_utf8(buffer).expect("renderer output is UTF-8");
        assert!(
            output.contains("evil"),
            "the build line must have drawn for the assertion to bite: {output:?}"
        );
        assert!(
            !output.contains('\u{1b}'),
            "escape byte must not reach the terminal: {output:?}"
        );
    }

    #[test]
    fn action_runtime_ensure_prints_a_phase_line_off_a_tty() {
        let output = drive(
            false,
            vec![
                ProgressEvent::ActionRuntimeEnsureStarted,
                ProgressEvent::ActionRuntimeEnsureFinished,
            ],
        );
        assert!(output.contains("action-runtime-ensure: pinned Node runtime ready\n"));
        assert!(!output.contains('\r'));
    }

    #[test]
    fn counts_and_bytes_format_for_humans() {
        assert_eq!(fmt_count(12345), "12,345");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 + 400 * 1024), "3.4 MiB");
        assert_eq!(fmt_bytes(1024 * 1024 * 1024 + 200 * 1024 * 1024), "1.2 GiB");
    }
}
