//! Source-location types preserved on every node of the typed workflow
//! model, so later phases (the CLI, `greenlit-engine`) can render errors as
//! `file:line:col — message — fix` (see `AGENTS.md` "greenlit-app (CLI
//! skeleton)").

use std::fmt;
use std::sync::Arc;

/// A 1-based line/column position within a workflow source file.
///
/// Both fields are 1-based to match GitHub's own error message convention
/// (design memo §6.1: `The value 'x' on line 5 and column 3 is invalid for
/// the type 'integer'`, from `YamlObjectReader.cs`). The underlying YAML
/// parser (`saphyr-parser`) reports a 1-based line but a **0-based** column
/// internally — its own doc comment claims "1-indexed" for both, but the
/// scanner initializes `col` to `0` and the crate's own `ScanError` display
/// impl adds one before printing (see
/// `saphyr-parser-0.0.11/src/scanner.rs` `Marker`/`ScanError`) — so every
/// [`Location`] built from a `saphyr_parser::Marker` in this crate adds one
/// to the raw column. This normalization lives in exactly one place
/// (`crate::yaml::raw::location_from_marker`) per the design memo's
/// mitigation guidance for saphyr's pre-1.0 API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number.
    pub column: u32,
}

impl Location {
    /// Construct a location from already-1-based coordinates.
    #[must_use]
    pub fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// A half-open range of source positions within one workflow file.
///
/// `file` is a cheaply-clonable path string (repo-relative, e.g.
/// `.github/workflows/ci.yml`) so every node can carry its own copy without
/// an arena or lifetime plumbing through the whole model.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    /// The workflow file this span points into.
    pub file: Arc<str>,
    /// Inclusive start position.
    pub start: Location,
    /// Exclusive end position.
    pub end: Location,
}

impl Span {
    /// Construct a new span.
    #[must_use]
    pub fn new(file: Arc<str>, start: Location, end: Location) -> Self {
        Self { file, start, end }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.start)
    }
}

/// A value paired with the source span it was parsed from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Spanned<T> {
    /// The parsed value.
    pub value: T,
    /// Where `value` came from in the source file.
    pub span: Span,
}

impl<T> Spanned<T> {
    /// Pair a value with its span.
    #[must_use]
    pub fn new(value: T, span: Span) -> Self {
        Self { value, span }
    }

    /// Transform the wrapped value, keeping the same span.
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Spanned<U> {
        Spanned {
            value: f(self.value),
            span: self.span,
        }
    }
}

impl<T> std::ops::Deref for Spanned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}
