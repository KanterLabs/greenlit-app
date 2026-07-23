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
            let candidate = candidate.to_string();
            if !self.masks.contains(&candidate) {
                self.masks.push(candidate);
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
