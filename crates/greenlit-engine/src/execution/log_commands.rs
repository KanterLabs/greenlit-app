//! Parser for GitHub's `::workflow-command::` log directives and the running
//! secret [`Masker`].
//!
//! A step's stdout/stderr is scanned line by line. A line of the form
//! `::command parameters::value` is a workflow command rather than plain
//! output. Greenlit v0 recognizes the grouping, annotation, and masking
//! commands. Masking (`::add-mask::`) takes effect immediately, so every line
//! emitted *after* the command is redacted.
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands>

use std::sync::{Arc, RwLock};
use std::{error::Error, fmt};

/// The token GitHub substitutes for a masked value in log output.
pub const MASK_TOKEN: &str = "***";

/// The only diagnostic exposed when sensitive-value registration cannot be
/// completed safely.
///
/// The rejected value and its dimensions are deliberately absent: this text
/// can be rendered and retained even when the rejected value was itself a
/// credential.
pub const MASK_REGISTRATION_FAILURE_DIAGNOSTIC: &str = "sensitive-value registration failed; subsequent output was suppressed\n  fix: use a mask of at least four bytes and reduce the number or size of masks, then retry";

const MIN_DYNAMIC_VALUE_BYTES: usize = 4;
const MAX_SENSITIVE_VALUES: usize = 256;
const MAX_SENSITIVE_VALUE_BYTES: usize = 64 * 1024;
const MAX_PATTERN_COUNT: usize = 1_024;
const MAX_PATTERN_BYTES: usize = 4 * 1024 * 1024;

/// One recognized workflow command, or a plain output line.
#[derive(Clone, PartialEq, Eq)]
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

impl fmt::Debug for LogLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Output(_) => "Output",
            Self::StartGroup(_) => "StartGroup",
            Self::EndGroup => "EndGroup",
            Self::Error(_) => "Error",
            Self::Warning(_) => "Warning",
            Self::Notice(_) => "Notice",
            Self::Debug(_) => "Debug",
            Self::AddMask(_) => "AddMask",
        };
        formatter.debug_tuple(name).field(&"[redacted]").finish()
    }
}

/// The message of an annotation command. GitHub also carries `file`,
/// `line`, `col`, and `title` parameters; v0 keeps the raw parameter string
/// so nothing is lost, plus the decoded message.
#[derive(Clone, PartialEq, Eq)]
pub struct Annotation {
    /// The raw, still-encoded parameter string between the command name and
    /// the `::` (empty when no parameters were given).
    pub parameters: String,
    /// The decoded annotation message.
    pub message: String,
}

impl fmt::Debug for Annotation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Annotation")
            .field("parameters", &"[redacted]")
            .field("message", &"[redacted]")
            .finish()
    }
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

/// A bounded, in-memory authority for values that must never reach rendered
/// or retained output.
///
/// Clones address the same synchronized state. The type intentionally has no
/// `Debug` or serialization implementation because it owns raw sensitive
/// values. Callers may take an in-memory snapshot for a final retained-tree
/// scan, but must never persist that snapshot.
#[derive(Clone)]
pub struct SensitiveValueRegistry {
    inner: Arc<RwLock<SensitiveValueState>>,
}

struct SensitiveValueState {
    /// Original registered values. Encodings are derived again by retained
    /// scanners rather than being serialized from this registry.
    values: Vec<String>,
    value_bytes: usize,
    /// Longest first, so overlapping masks redact the widest match.
    patterns: Vec<String>,
    pattern_bytes: usize,
    failed: bool,
    failure_diagnostic: String,
}

impl Default for SensitiveValueState {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            value_bytes: 0,
            patterns: Vec::new(),
            pattern_bytes: 0,
            failed: false,
            failure_diagnostic: MASK_REGISTRATION_FAILURE_DIAGNOSTIC.to_string(),
        }
    }
}

/// A fallible, opaque in-memory view of all accepted sensitive source values.
///
/// This type deliberately implements neither `Debug` nor serialization.
/// Consumers use it only while screening bytes that are about to be retained.
pub struct SensitiveValueSnapshot {
    values: Vec<String>,
}

impl SensitiveValueSnapshot {
    /// Borrows the accepted values for an in-memory retained-byte scan.
    pub fn values(&self) -> &[String] {
        &self.values
    }
}

impl Default for SensitiveValueRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(SensitiveValueState::default())),
        }
    }
}

impl SensitiveValueRegistry {
    /// Creates an empty run-level registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a value known before workflow output begins.
    ///
    /// Empty and whitespace-only values retain the historical no-op behavior.
    /// All other values and their bounded common encodings are registered
    /// atomically. A failure latches the registry into its fail-closed state.
    ///
    /// # Errors
    ///
    /// Returns a fixed, value-independent error if a safety bound is exceeded
    /// or the synchronized registry cannot be trusted.
    pub fn register(&self, value: &str) -> Result<(), MaskRegistrationError> {
        self.register_inner(value, RegistrationKind::Initial)
    }

    /// Registers a workflow-provided `::add-mask::` value.
    ///
    /// Dynamic values must be non-whitespace and every non-empty line must be
    /// at least four bytes. Short masks are rejected because their bounded
    /// common encodings cannot be retained safely without matching ordinary
    /// output pervasively. Rejection latches the whole run authority failed.
    ///
    /// # Errors
    ///
    /// Returns a fixed, value-independent error for invalid input, an exceeded
    /// bound, or an untrusted synchronized registry.
    pub fn register_dynamic(&self, value: &str) -> Result<(), MaskRegistrationError> {
        self.register_inner(value, RegistrationKind::Dynamic)
    }

    /// Returns a healthy snapshot for an in-memory retained-tree scan.
    ///
    /// The returned values remain sensitive and must never be persisted.
    ///
    /// # Errors
    ///
    /// Returns the fixed, already-redacted registration error instead of an
    /// empty snapshot when the authority has failed or its lock was poisoned.
    pub fn healthy_snapshot(&self) -> Result<SensitiveValueSnapshot, MaskRegistrationError> {
        match self.inner.read() {
            Ok(state) if !state.failed => Ok(SensitiveValueSnapshot {
                values: state.values.clone(),
            }),
            Ok(state) => Err(state.error()),
            Err(poisoned) => {
                drop(poisoned.into_inner());
                Err(self.latch_failed())
            }
        }
    }

    /// Fails if any registration or synchronization error has made this
    /// authority unsafe for terminal publication.
    ///
    /// # Errors
    ///
    /// Returns the fixed registration error after the failure latch is set.
    pub fn ensure_healthy(&self) -> Result<(), MaskRegistrationError> {
        match self.inner.read() {
            Ok(state) if !state.failed => Ok(()),
            Ok(state) => Err(state.error()),
            Err(poisoned) => {
                drop(poisoned.into_inner());
                Err(self.latch_failed())
            }
        }
    }

    fn register_inner(
        &self,
        value: &str,
        kind: RegistrationKind,
    ) -> Result<(), MaskRegistrationError> {
        if value.trim().is_empty() {
            return match kind {
                RegistrationKind::Initial => Ok(()),
                RegistrationKind::Dynamic => self.fail(value),
            };
        }
        if value.len() > MAX_SENSITIVE_VALUE_BYTES {
            return self.fail(value);
        }
        if kind == RegistrationKind::Dynamic
            && std::iter::once(value)
                .chain(value.lines())
                .filter(|candidate| !candidate.is_empty())
                .any(|candidate| {
                    candidate.trim().is_empty() || candidate.len() < MIN_DYNAMIC_VALUE_BYTES
                })
        {
            return self.fail(value);
        }

        let candidates = match mask_candidates(value) {
            Ok(candidates) => candidates,
            Err(_) => return self.fail(value),
        };
        let mut state = match self.inner.write() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                return Err(state.fail(Some(value)));
            }
        };
        if state.failed {
            return Err(state.fail(Some(value)));
        }
        if state.values.iter().any(|registered| registered == value) {
            return Ok(());
        }

        let additions = candidates
            .into_iter()
            .filter(|candidate| !state.patterns.contains(candidate))
            .collect::<Vec<_>>();
        let added_pattern_bytes = additions.iter().try_fold(0_usize, |total, candidate| {
            total.checked_add(candidate.len())
        });
        let value_count = state.values.len().checked_add(1);
        let value_bytes = state.value_bytes.checked_add(value.len());
        let pattern_count = state.patterns.len().checked_add(additions.len());
        let pattern_bytes =
            added_pattern_bytes.and_then(|added| state.pattern_bytes.checked_add(added));
        if value_count.is_none_or(|count| count > MAX_SENSITIVE_VALUES)
            || value_bytes.is_none_or(|bytes| bytes > MAX_PATTERN_BYTES)
            || pattern_count.is_none_or(|count| count > MAX_PATTERN_COUNT)
            || pattern_bytes.is_none_or(|bytes| bytes > MAX_PATTERN_BYTES)
        {
            return Err(state.fail(Some(value)));
        }

        state.values.push(value.to_string());
        state.values.sort();
        state.value_bytes = value_bytes.unwrap_or_default();
        state.patterns.extend(additions);
        state
            .patterns
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        state.pattern_bytes = pattern_bytes.unwrap_or_default();
        Ok(())
    }

    fn fail<T>(&self, rejected: &str) -> Result<T, MaskRegistrationError> {
        let error = match self.inner.write() {
            Ok(mut state) => state.fail(Some(rejected)),
            Err(poisoned) => poisoned.into_inner().fail(Some(rejected)),
        };
        Err(error)
    }

    fn latch_failed(&self) -> MaskRegistrationError {
        match self.inner.write() {
            Ok(mut state) => state.fail(None),
            Err(poisoned) => poisoned.into_inner().fail(None),
        }
    }
}

impl SensitiveValueState {
    fn error(&self) -> MaskRegistrationError {
        MaskRegistrationError {
            diagnostic: self.failure_diagnostic.clone(),
        }
    }

    fn fail(&mut self, rejected: Option<&str>) -> MaskRegistrationError {
        self.failure_diagnostic =
            sanitized_failure_diagnostic(&self.patterns, rejected, &self.failure_diagnostic);
        self.failed = true;
        self.error()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegistrationKind {
    Initial,
    Dynamic,
}

/// Fixed, value-independent sensitive-value registration failure.
#[derive(Clone, PartialEq, Eq)]
pub struct MaskRegistrationError {
    diagnostic: String,
}

impl fmt::Debug for MaskRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MaskRegistrationError")
    }
}

impl fmt::Display for MaskRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl Error for MaskRegistrationError {}

/// Accumulates values that must be redacted from all subsequent output, and
/// applies the redaction. Fed by `::add-mask::` commands and by every
/// registered secret value at the start of a run.
///
/// GitHub masks the exact string, and — for a multiline value — each of its
/// individual lines, so a partial echo of a secret is still redacted.
/// <https://docs.github.com/en/actions/security-for-github-actions/security-guides/using-secrets-in-github-actions#masking-secrets>
///
/// Clones share a [`SensitiveValueRegistry`], making accepted dynamic masks
/// visible immediately across parallel jobs and caller-side renderers.
#[derive(Clone, Default)]
pub struct Masker {
    registry: SensitiveValueRegistry,
}

impl Masker {
    /// A masker with no registered values.
    pub fn new() -> Self {
        Masker::default()
    }

    /// Creates a masker backed by `registry`.
    pub fn with_registry(registry: SensitiveValueRegistry) -> Self {
        Self { registry }
    }

    /// Returns the shared in-memory registry backing this masker.
    pub fn registry(&self) -> SensitiveValueRegistry {
        self.registry.clone()
    }

    /// Registers a value to redact. Empty or whitespace-only values are
    /// ignored (GitHub does not mask them, to avoid redacting every space in
    /// the log). Both the full value and each of its non-empty lines are
    /// registered.
    ///
    /// # Errors
    ///
    /// Returns a fixed, value-independent error if a safety bound is exceeded.
    pub fn add(&self, value: &str) -> Result<(), MaskRegistrationError> {
        self.registry.register(value)
    }

    /// Registers an untrusted workflow `::add-mask::` value.
    ///
    /// # Errors
    ///
    /// Returns a fixed, value-independent error if the value is invalid or a
    /// safety bound is exceeded.
    pub fn add_dynamic(&self, value: &str) -> Result<(), MaskRegistrationError> {
        self.registry.register_dynamic(value)
    }

    /// Returns a healthy, opaque snapshot for an in-memory retained-tree scan.
    ///
    /// # Errors
    ///
    /// Returns the fixed registration error if this run-level authority has
    /// failed.
    pub fn healthy_snapshot(&self) -> Result<SensitiveValueSnapshot, MaskRegistrationError> {
        self.registry.healthy_snapshot()
    }

    /// Refuses terminal publication after any registration failure.
    ///
    /// # Errors
    ///
    /// Returns the fixed registration error if this run-level authority has
    /// failed.
    pub fn ensure_healthy(&self) -> Result<(), MaskRegistrationError> {
        self.registry.ensure_healthy()
    }

    /// Latches the authority failed after a surrounding redaction boundary
    /// can no longer guarantee bounded processing.
    ///
    /// The returned error contains only the fixed, already-redacted
    /// diagnostic.
    pub fn fail_closed(&self) -> MaskRegistrationError {
        self.registry.latch_failed()
    }

    /// Whether any value is registered.
    pub fn is_empty(&self) -> bool {
        match self.registry.inner.read() {
            Ok(state) => state.patterns.is_empty(),
            Err(poisoned) => {
                drop(poisoned.into_inner());
                let _ = self.registry.latch_failed();
                false
            }
        }
    }

    /// Returns `line` with every registered value replaced by [`MASK_TOKEN`].
    pub fn apply(&self, line: &str) -> String {
        let state = match self.registry.inner.read() {
            Ok(state) if !state.failed => state,
            Ok(state) => return state.failure_diagnostic.clone(),
            Err(poisoned) => {
                drop(poisoned.into_inner());
                return self.registry.latch_failed().to_string();
            }
        };
        let mut redacted = line.to_string();
        for mask in &state.patterns {
            if redacted.contains(mask.as_str()) {
                redacted = redacted.replace(mask.as_str(), MASK_TOKEN);
            }
        }
        redacted
    }
}

fn mask_candidates(value: &str) -> Result<Vec<String>, MaskRegistrationError> {
    let mut candidates = Vec::new();
    let mut pattern_bytes = 0_usize;
    for candidate in std::iter::once(value).chain(value.lines()) {
        if candidate.trim().is_empty() {
            continue;
        }
        let variants = encoded_variants(candidate);
        for variant in std::iter::once(candidate.to_string()).chain(variants) {
            push_candidate(variant, &mut candidates, &mut pattern_bytes)?;
        }
    }
    Ok(candidates)
}

fn push_candidate(
    candidate: String,
    candidates: &mut Vec<String>,
    pattern_bytes: &mut usize,
) -> Result<(), MaskRegistrationError> {
    if candidates.contains(&candidate) {
        return Ok(());
    }
    let next_bytes =
        pattern_bytes
            .checked_add(candidate.len())
            .ok_or_else(|| MaskRegistrationError {
                diagnostic: MASK_REGISTRATION_FAILURE_DIAGNOSTIC.to_string(),
            })?;
    if candidates.len() >= MAX_PATTERN_COUNT || next_bytes > MAX_PATTERN_BYTES {
        return Err(MaskRegistrationError {
            diagnostic: MASK_REGISTRATION_FAILURE_DIAGNOSTIC.to_string(),
        });
    }
    *pattern_bytes = next_bytes;
    candidates.push(candidate);
    Ok(())
}

fn sanitized_failure_diagnostic(
    patterns: &[String],
    rejected: Option<&str>,
    current: &str,
) -> String {
    let mut diagnostic = current.to_string();
    for pattern in patterns {
        diagnostic = diagnostic.replace(pattern, MASK_TOKEN);
    }
    if let Some(value) = rejected {
        for candidate in std::iter::once(value).chain(value.lines()) {
            if candidate.is_empty() {
                continue;
            }
            diagnostic = diagnostic.replace(candidate, MASK_TOKEN);
            if candidate.len() <= MAX_SENSITIVE_VALUE_BYTES {
                for variant in encoded_variants(candidate) {
                    diagnostic = diagnostic.replace(&variant, MASK_TOKEN);
                }
            }
        }
    }
    diagnostic
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
