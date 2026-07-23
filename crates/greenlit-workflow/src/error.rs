//! The parser's single error type.

use crate::span::Span;
use std::sync::Arc;

/// Everything that can go wrong parsing a `.github/workflows/*.yml` file
/// into the typed model, from a raw YAML syntax error through a workflow
/// schema violation.
///
/// Each variant carries enough structured data (a [`Span`] wherever the
/// underlying YAML has one) for a caller (the `litci` CLI) to render
/// `file:line:col — message — fix` per `AGENTS.md`'s UX rule; [`ParseError::span`]
/// exposes it uniformly.
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum ParseError {
    /// [`crate::parse::parse_workflow_file`] could not read the file.
    #[error("{path}: could not read workflow file: {message}")]
    Io {
        /// The path that could not be read.
        path: Arc<str>,
        /// The underlying I/O error's message.
        message: String,
    },

    /// The workflow file was readable but was not valid UTF-8. GitHub
    /// workflow source is UTF-8 text, so this is distinct from a filesystem
    /// access failure and needs a different user action.
    #[error("{path}: workflow file is not valid UTF-8: {message}")]
    Encoding {
        /// The path whose contents could not be decoded.
        path: Arc<str>,
        /// The UTF-8 decoder's diagnostic.
        message: String,
    },

    /// The workflow source exceeds GitHub's default 1 MiB character limit.
    /// The runner measures .NET `String.Length`, i.e. UTF-16 code units.
    #[error("{path}: workflow file exceeds GitHub's {max_characters}-character YAML limit")]
    SourceLimit {
        /// The workflow file name.
        path: Arc<str>,
        /// GitHub's maximum accepted UTF-16 code-unit count.
        max_characters: usize,
    },

    /// The input was not well-formed YAML at all (unterminated string,
    /// bad indentation, unknown anchor, …). Reported at a bare `line:column`
    /// (not a [`Span`]) because the scanner only gives a single position for
    /// these failures.
    #[error("{path}:{line}:{column}: YAML syntax error: {message}")]
    Yaml {
        /// The workflow file path.
        path: Arc<str>,
        /// 1-based line.
        line: u32,
        /// 1-based column.
        column: u32,
        /// The scanner's message.
        message: String,
    },

    /// A mapping contained a key this parser does not recognize in that
    /// position. This includes keys GitHub itself would accept but v0 does
    /// not model (see the crate-level docs "Unknown-key policy") as well as
    /// genuine typos.
    #[error("{span}: unknown key '{key}' in {context}")]
    UnknownKey {
        /// Where the offending key appears.
        span: Span,
        /// The key text as written.
        key: String,
        /// A short description of the enclosing construct, e.g. `"job 'build'"`.
        context: String,
    },

    /// A required key was missing from a mapping (e.g. a job with no
    /// `runs-on`, a step with neither `run` nor `uses`).
    #[error("{span}: missing required key '{key}' in {context}")]
    MissingKey {
        /// The span of the enclosing mapping.
        span: Span,
        /// The missing key's name.
        key: &'static str,
        /// A short description of the enclosing construct.
        context: String,
    },

    /// A node had the wrong YAML shape for its position (e.g. a scalar
    /// where a mapping was required), or a workflow-level structural rule
    /// was violated (e.g. a job with both `run`-steps and `uses`).
    #[error("{span}: {message}")]
    Schema {
        /// Where the violation was found.
        span: Span,
        /// A human-readable description.
        message: String,
    },

    /// YAML traversal exceeded one of GitHub's parser resource limits.
    #[error("{span}: {message}")]
    YamlLimit {
        /// The node that crossed the limit.
        span: Span,
        /// Which runner-compatible limit was exceeded.
        message: String,
    },

    /// A `${{ ... }}` template was not closed, its inner expression did
    /// not parse, or it referenced a context/special function unavailable
    /// at the workflow key where it appeared. GitHub validates all three at
    /// workflow-read time; keeping the modeled field's span here prevents a
    /// malformed expression from disappearing during static extraction.
    #[error("{span}: invalid expression in {context}: {message}")]
    Expression {
        /// The complete scalar containing the invalid expression.
        span: Span,
        /// The workflow key whose expression policy applies.
        context: String,
        /// The delimiter, grammar, context, or function error.
        message: String,
    },

    /// A scalar used an explicit YAML tag other than the five honored
    /// core-schema scalar tags (`!!str`, `!!bool`, `!!int`, `!!float`,
    /// `!!null`). Collection tags are a separate runner behavior: every tag
    /// on a sequence or mapping is ignored. This follows the runner's
    /// workflow YAML reader:
    /// <https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ObjectTemplating/YamlObjectReader.cs>.
    #[error("{span}: unsupported YAML tag '{tag}'")]
    UnsupportedTag {
        /// Where the tag appears.
        span: Span,
        /// The tag as written.
        tag: String,
    },

    /// A scalar carried one of the four honored core-schema tags, but its
    /// text does not parse as that type (e.g. `!!bool maybe`).
    #[error("{span}: value '{raw}' does not conform to explicit tag '!!{tag_name}'")]
    TagMismatch {
        /// Where the scalar appears.
        span: Span,
        /// The scalar's raw text.
        raw: String,
        /// The tag suffix that was not honored (`"bool"`, `"int"`, …).
        tag_name: &'static str,
    },

    /// A non-string explicit YAML tag was attached to a quoted or block
    /// scalar. The runner accepts `!!bool`, `!!int`, `!!float`, and `!!null`
    /// only on plain-style scalars; `!!str` remains valid for every scalar
    /// style.
    #[error("{span}: scalar style '{style}' is not valid with explicit tag '!!{tag_name}'")]
    InvalidTagStyle {
        /// Where the tagged scalar appears.
        span: Span,
        /// The scalar style rendered for a user-facing diagnostic.
        style: &'static str,
        /// The explicit core-tag suffix (`"bool"`, `"int"`, …).
        tag_name: &'static str,
    },

    /// A hex/octal integer literal's magnitude does not fit in the runner's
    /// signed 32-bit representation. See `MatchInteger` in
    /// <https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ObjectTemplating/YamlObjectReader.cs>.
    #[error("{span}: integer literal '{raw}' is out of i32 range")]
    IntegerOverflow {
        /// Where the literal appears.
        span: Span,
        /// The literal's raw text.
        raw: String,
    },

    /// The same mapping key appeared twice. GitHub's template reader rejects
    /// duplicate mapping keys outright; see `TemplateReader`'s key set in
    /// <https://github.com/actions/runner/blob/main/src/Sdk/DTObjectTemplating/ObjectTemplating/TemplateReader.cs>
    /// and `yaml::raw`'s module docs for the exact rule implemented here.
    #[error("{span}: duplicate mapping key '{key}' (first seen at {first_span})")]
    DuplicateKey {
        /// The second (rejected) occurrence.
        span: Span,
        /// The key text.
        key: String,
        /// Where the key first appeared.
        first_span: Span,
    },

    /// The file contained more than one `---`-separated YAML document.
    /// GitHub workflow files are exactly one document.
    #[error("{path}: workflow file must contain exactly one YAML document, found {count}")]
    MultipleDocuments {
        /// The workflow file path.
        path: Arc<str>,
        /// How many documents were found.
        count: usize,
    },

    /// The file contained no YAML document at all (empty file, or a
    /// document with no content).
    #[error("{path}: workflow file contains no YAML document")]
    EmptyDocument {
        /// The workflow file path.
        path: Arc<str>,
    },
}

impl ParseError {
    /// The source span this error points at, when one is available.
    ///
    /// Absent for the file-level variants ([`ParseError::Yaml`] — which
    /// only has a bare position, not a range — [`ParseError::MultipleDocuments`],
    /// [`ParseError::EmptyDocument`]).
    #[must_use]
    pub fn span(&self) -> Option<&Span> {
        match self {
            ParseError::UnknownKey { span, .. }
            | ParseError::MissingKey { span, .. }
            | ParseError::Schema { span, .. }
            | ParseError::YamlLimit { span, .. }
            | ParseError::Expression { span, .. }
            | ParseError::UnsupportedTag { span, .. }
            | ParseError::TagMismatch { span, .. }
            | ParseError::InvalidTagStyle { span, .. }
            | ParseError::IntegerOverflow { span, .. }
            | ParseError::DuplicateKey { span, .. } => Some(span),
            ParseError::Io { .. }
            | ParseError::Encoding { .. }
            | ParseError::SourceLimit { .. }
            | ParseError::Yaml { .. }
            | ParseError::MultipleDocuments { .. }
            | ParseError::EmptyDocument { .. } => None,
        }
    }
}
