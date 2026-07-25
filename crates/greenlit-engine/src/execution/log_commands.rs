//! Parser for GitHub's `::workflow-command::` log directives and the running
//! secret [`Masker`].
//!
//! A step's stdout/stderr is scanned line by line. A line of the form
//! `::command parameters::value` is a workflow command rather than plain
//! output. Greenlit v0 recognizes the grouping, annotation, and masking
//! commands. Masking (`::add-mask::`) takes effect immediately, so every line
//! emitted *after* the command is redacted.
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands>

/// The token GitHub substitutes for a masked value in log output.
pub const MASK_TOKEN: &str = "***";

/// One recognized workflow command, or a plain output line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLine {
    /// A normal output line (not a workflow command).
    Output(String),
    /// `::group::TITLE` — begins a collapsible log group.
    StartGroup(String),
    /// `::endgroup::` — ends the current group.
    EndGroup,
    /// `::error ...::message` — an error annotation.
    Error(Annotation),
    /// `::warning ...::message` — a warning annotation.
    Warning(Annotation),
    /// `::notice ...::message` — a notice annotation.
    Notice(Annotation),
    /// `::debug::message` — a debug line (shown only when debug logging is on).
    Debug(String),
    /// `::add-mask::value` — registers a value to redact from later output.
    AddMask(String),
}

/// The message of an annotation command. GitHub also carries `file`,
/// `line`, `col`, and `title` parameters; v0 keeps the raw parameter string
/// so nothing is lost, plus the decoded message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// The raw, still-encoded parameter string between the command name and
    /// the `::` (empty when no parameters were given).
    pub parameters: String,
    /// The decoded annotation message.
    pub message: String,
}

/// Parses one raw output line into a [`LogLine`]. A line that does not match
/// the `::command::` grammar is returned as [`LogLine::Output`] verbatim.
pub fn parse_line(line: &str) -> LogLine {
    let Some(rest) = line.strip_prefix("::") else {
        return LogLine::Output(line.to_string());
    };
    // The command token runs up to the first `::` (which closes the command
    // and introduces the value) or, for parameterless forms, is the whole
    // token. Split on the first `::` separating command+params from value.
    let (head, value) = match rest.split_once("::") {
        Some((head, value)) => (head, value),
        None => (rest, ""),
    };
    // `head` is `command` or `command parameters`; the command name ends at
    // the first space.
    let (name, parameters) = match head.split_once(' ') {
        Some((name, params)) => (name, params),
        None => (head, ""),
    };
    match name {
        "group" => LogLine::StartGroup(decode_data(value)),
        "endgroup" => LogLine::EndGroup,
        "add-mask" => LogLine::AddMask(decode_data(value)),
        "debug" => LogLine::Debug(decode_data(value)),
        "error" => LogLine::Error(annotation(parameters, value)),
        "warning" => LogLine::Warning(annotation(parameters, value)),
        "notice" => LogLine::Notice(annotation(parameters, value)),
        // An unrecognized `::name::` is not a command Greenlit acts on; keep
        // the original line as output so nothing is silently dropped.
        _ => LogLine::Output(line.to_string()),
    }
}

fn annotation(parameters: &str, value: &str) -> Annotation {
    Annotation {
        parameters: parameters.to_string(),
        message: decode_data(value),
    }
}

/// Decodes the message/property escaping GitHub applies to workflow-command
/// data: `%25`→`%`, `%0D`→`\r`, `%0A`→`\n`.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands#example-masking-and-passing-a-value-between-jobs>
fn decode_data(value: &str) -> String {
    value
        .replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%25", "%")
}

/// Accumulates values that must be redacted from all subsequent output, and
/// applies the redaction. Fed by `::add-mask::` commands and by every
/// registered secret value at the start of a run.
///
/// GitHub masks the exact string, and — for a multiline value — each of its
/// individual lines, so a partial echo of a secret is still redacted.
/// <https://docs.github.com/en/actions/security-for-github-actions/security-guides/using-secrets-in-github-actions#masking-secrets>
#[derive(Debug, Clone, Default)]
pub struct Masker {
    /// Longest first, so overlapping masks redact the widest match.
    masks: Vec<String>,
}

impl Masker {
    /// A masker with no registered values.
    pub fn new() -> Self {
        Masker::default()
    }

    /// Registers a value to redact. Empty or whitespace-only values are
    /// ignored (GitHub does not mask them, to avoid redacting every space in
    /// the log). Both the full value and each of its non-empty lines are
    /// registered.
    pub fn add(&mut self, value: &str) {
        for candidate in std::iter::once(value).chain(value.lines()) {
            if candidate.trim().is_empty() {
                continue;
            }
            let variants = encoded_variants(candidate);
            for variant in std::iter::once(candidate.to_string()).chain(variants) {
                if !self.masks.contains(&variant) {
                    self.masks.push(variant);
                }
            }
        }
        // Redact wider matches first so a full multiline secret is replaced
        // before its component lines.
        self.masks.sort_by_key(|mask| std::cmp::Reverse(mask.len()));
    }

    /// Whether any value is registered.
    pub fn is_empty(&self) -> bool {
        self.masks.is_empty()
    }

    /// Returns `line` with every registered value replaced by [`MASK_TOKEN`].
    pub fn apply(&self, line: &str) -> String {
        let mut redacted = line.to_string();
        for mask in &self.masks {
            if redacted.contains(mask.as_str()) {
                redacted = redacted.replace(mask.as_str(), MASK_TOKEN);
            }
        }
        redacted
    }
}

fn encoded_variants(value: &str) -> Vec<String> {
    // Very short encodings are common substrings and would erase unrelated
    // log text. GitHub recommends registering transformed secrets explicitly;
    // Greenlit additionally covers the common transport encodings for values
    // long enough to remain specific.
    if value.len() < 4 {
        return Vec::new();
    }
    let standard = base64(value.as_bytes(), b'+', b'/', true);
    let url = base64(value.as_bytes(), b'-', b'_', false);
    let percent = percent_encode(value.as_bytes());
    let mut variants = vec![standard, url];
    if percent != value {
        variants.push(percent);
    }
    variants
}

fn base64(bytes: &[u8], char62: u8, char63: u8, padded: bool) -> String {
    let mut alphabet = *b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    alphabet[62] = char62;
    alphabet[63] = char63;
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or_default();
        let third = chunk.get(2).copied().unwrap_or_default();
        encoded.push(char::from(alphabet[usize::from(first >> 2)]));
        encoded.push(char::from(
            alphabet[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        if chunk.len() > 1 {
            encoded.push(char::from(
                alphabet[usize::from(((second & 0x0f) << 2) | (third >> 6))],
            ));
        } else if padded {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(char::from(alphabet[usize::from(third & 0x3f)]));
        } else if padded {
            encoded.push('=');
        }
    }
    encoded
}

fn percent_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(*byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}
