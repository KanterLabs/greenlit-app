//! Escaping at human-facing terminal output boundaries.
//!
//! Workflow files are untrusted input. Names, scripts, expressions, source
//! paths, and diagnostics may therefore contain terminal control characters
//! (including YAML `\e` escapes). Writing those characters verbatim would let
//! a repository clear or rewrite the visible plan, create deceptive links, or
//! emit OSC commands. Human output preserves only layout characters written
//! by the renderer; dynamic inline values escape every control character.
//! Bidirectional text controls are also escaped so a name cannot visually
//! reorder trusted labels around it.

use std::io::Write;

/// A resolved Phase 1 plan is already capped at 64 MiB. Human formatting can
/// add labels and escaping overhead, so allow twice that while still keeping
/// a malicious repository from driving an unbounded renderer allocation.
const MAX_HUMAN_RENDER_BYTES: usize = 128 * 1024 * 1024;

pub(crate) struct BoundedRenderBuffer {
    bytes: Vec<u8>,
}

impl BoundedRenderBuffer {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }
}

impl Write for BoundedRenderBuffer {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if input.len() > MAX_HUMAN_RENDER_BYTES.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other(format!(
                "human output exceeds Greenlit's {} MiB safety limit",
                MAX_HUMAN_RENDER_BYTES / (1024 * 1024)
            )));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Buffers one renderer's UTF-8 output, then writes it with terminal control
/// characters escaped. Buffering keeps UTF-8 decoding correct even when the
/// renderer performs many small `write!` calls.
pub(crate) fn render_sanitized(
    out: &mut impl Write,
    render: impl FnOnce(&mut BoundedRenderBuffer) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut buffer = BoundedRenderBuffer::new();
    render(&mut buffer)?;
    let text = String::from_utf8(buffer.bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("human renderer produced invalid UTF-8: {error}"),
        )
    })?;
    write_sanitized(out, &text)
}

/// Writes `text`, preserving renderer layout while making terminal controls
/// inert and visible with Rust-style Unicode escapes such as `\u{1b}`.
pub(crate) fn write_sanitized(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    const OUTPUT_CHUNK_BYTES: usize = 8 * 1024;

    let mut safe = String::with_capacity(OUTPUT_CHUNK_BYTES);
    for character in text.chars() {
        if (character == '\n' || character == '\t' || !character.is_control())
            && !is_bidi_control(character)
        {
            safe.push(character);
        } else {
            safe.extend(character.escape_default());
        }
        if safe.len() >= OUTPUT_CHUNK_BYTES {
            out.write_all(safe.as_bytes())?;
            safe.clear();
        }
    }
    if !safe.is_empty() {
        out.write_all(safe.as_bytes())?;
    }
    Ok(())
}

/// Escapes an untrusted value for use within one trusted renderer line.
/// Newlines and tabs are escaped here, unlike [`write_sanitized`], because
/// they came from the value rather than renderer layout.
pub(crate) fn inline_escape(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() || is_bidi_control(character) {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}
