//! The span-preserving raw YAML tree, and the event-driven builder that
//! produces it.
//!
//! This is the one module in the crate that touches `saphyr_parser` types,
//! confining the pre-1.0 dependency surface to one small internal module.
//! It drives the parser's low-level event stream
//! directly — rather than going through the higher-level `saphyr` crate's
//! `YamlLoader`/`MarkedYaml` — for one concrete reason: GitHub's workflow
//! YAML dialect rejects duplicate mapping keys outright and treats `<<` as
//! an ordinary (unsupported) key rather than performing a YAML-1.1 merge
//! as GitHub's `TemplateReader` does, and `YamlLoader`'s internal map-building
//! (`hashlink::LinkedHashMap::insert`, which silently overwrites a
//! duplicate key) is not something a caller can hook or override — its
//! `doc_stack`/`key_stack` fields are private. Building the tree from raw
//! events instead gives full control over both checks. Greenlit applies
//! that event-stream approach to `saphyr-parser`, the actively maintained
//! low-level half of the same project.
//!
//! Duplicate detection operates case-insensitively on each scalar key's
//! decoded string value, independent of whether YAML wrote it plain or
//! quoted. This matches the runner template reader, which converts a key to
//! `StringToken` and adds `nextKey.Value` to an `OrdinalIgnoreCase`
//! duplicate set without carrying scalar style:
//! <https://github.com/actions/runner/blob/main/src/Sdk/DTObjectTemplating/ObjectTemplating/TemplateReader.cs>.

use crate::error::ParseError;
use crate::span::{Location, Span, Spanned};
use crate::yaml::scalar::YamlScalar;
use saphyr_parser::{Marker, Parser, ScalarStyle};
use std::collections::HashMap;
use std::sync::Arc;

mod receiver;
mod state;

// The current GitHub runner rejects after traversing 50,000 YAML events,
// counting events replayed through aliases each time. Its outer template
// reader independently permits at most 50 nested collection elements.
// Pinned sources:
// https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/Conversion/YamlObjectReader.cs
// https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/ParseOptions.cs
const MAX_TRAVERSED_YAML_NODES: usize = 50_000;
const MAX_YAML_DEPTH: usize = 50;
const MAX_YAML_RESULT_BYTES: usize = 10 * 1024 * 1024;
const MIN_TOKEN_BYTES: usize = 24;
const STRING_BASE_BYTES: usize = 26;

/// A YAML scalar node before workflow-schema interpretation: its raw text,
/// the YAML style it was written in, and its resolved [`YamlScalar`] value
/// (already honoring any explicit core-schema tag, per [`typed_scalar`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawScalar {
    pub raw: String,
    pub style: ScalarStyle,
    pub value: YamlScalar,
}

/// A YAML node with its structure preserved (but, per crate scope, no
/// resolution of workflow *semantics* yet — that is `crate::parse`'s job).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RawNode {
    Scalar(RawScalar),
    // Collections are shared because an alias is a replay of one already
    // parsed event subtree. Keeping the children behind `Arc` makes cloning
    // an anchored collection O(1), including when anchors are nested, while
    // preserving a distinct outer `Spanned` value for each alias use-site.
    // The runner's logical event/result budgets are still charged for every
    // replay before the shared node is materialized below.
    Sequence(Arc<[Spanned<RawNode>]>),
    Mapping(Arc<[(Spanned<RawNode>, Spanned<RawNode>)]>),
}

impl RawNode {
    /// The scalar's raw source text, for structural (key-like) uses that
    /// must ignore GitHub's null/bool/number scalar typing entirely (job
    /// ids, step ids, env var names, event names, …). Returns `None` for
    /// non-scalar nodes.
    pub(crate) fn as_key_text(&self) -> Option<&str> {
        match self {
            RawNode::Scalar(s) => Some(s.raw.as_str()),
            RawNode::Sequence(_) | RawNode::Mapping(_) => None,
        }
    }
}

/// Parse `source` (the contents of `file`) into a single raw YAML document
/// tree, applying GitHub's scalar-typing and duplicate-key rules while
/// doing so.
///
/// # Errors
/// Returns [`ParseError::Yaml`] for malformed YAML, [`ParseError::MultipleDocuments`]
/// / [`ParseError::EmptyDocument`] for a file that isn't exactly one
/// document, and any of the structural variants
/// ([`ParseError::UnsupportedTag`], [`ParseError::TagMismatch`],
/// [`ParseError::IntegerOverflow`], [`ParseError::DuplicateKey`],
/// [`ParseError::Schema`]) raised while building the tree.
pub(crate) fn parse_raw(file: Arc<str>, source: &str) -> Result<Spanned<RawNode>, ParseError> {
    let mut parser = Parser::new_from_str(source);
    let mut builder = Builder::new(file.clone());
    parser
        .load(&mut builder, true)
        .map_err(|scan_err| yaml_syntax_error(&file, &scan_err))?;
    if let Some(err) = builder.error {
        return Err(err);
    }
    match builder.documents.len() {
        1 => builder
            .documents
            .into_iter()
            .next()
            .flatten()
            .ok_or(ParseError::EmptyDocument { path: file }),
        0 => Err(ParseError::EmptyDocument { path: file }),
        count => Err(ParseError::MultipleDocuments { path: file, count }),
    }
}

fn yaml_syntax_error(file: &Arc<str>, e: &saphyr_parser::ScanError) -> ParseError {
    let m = e.marker();
    ParseError::Yaml {
        path: file.clone(),
        line: u32::try_from(m.line()).unwrap_or(u32::MAX),
        column: u32::try_from(m.col()).unwrap_or(u32::MAX).saturating_add(1),
        message: e.info().to_string(),
    }
}

fn location_from_marker(m: Marker) -> Location {
    // saphyr-parser's `Marker::col()` is 0-based despite its own doc
    // comment claiming "1-indexed" (its `line` field genuinely is 1-based;
    // its `ScanError` `Display` impl adds one to `col` before printing) —
    // see `crate::span::Location` docs for the full citation. Normalized to
    // 1-based here, in the one module that touches `Marker` directly.
    Location::new(
        u32::try_from(m.line()).unwrap_or(u32::MAX),
        u32::try_from(m.col()).unwrap_or(u32::MAX).saturating_add(1),
    )
}

fn span_from(file: &Arc<str>, s: saphyr_parser::Span) -> Span {
    Span::new(
        file.clone(),
        location_from_marker(s.start),
        location_from_marker(s.end),
    )
}

fn scalar_result_bytes(node: &RawNode) -> usize {
    let RawNode::Scalar(scalar) = node else {
        return MIN_TOKEN_BYTES;
    };
    match &scalar.value {
        YamlScalar::String(value) => MIN_TOKEN_BYTES
            .saturating_add(STRING_BASE_BYTES)
            .saturating_add(value.encode_utf16().count().saturating_mul(2)),
        YamlScalar::Null | YamlScalar::Bool(_) | YamlScalar::Number(_) => MIN_TOKEN_BYTES,
    }
}

/// One open sequence/mapping while walking the event stream.
enum Frame {
    Sequence {
        start: Marker,
        anchor_id: usize,
        items: Vec<Spanned<RawNode>>,
        event_count: usize,
        collection_depth: usize,
        result_bytes: usize,
    },
    Mapping {
        start: Marker,
        anchor_id: usize,
        pending_key: Option<Spanned<RawNode>>,
        entries: Vec<(Spanned<RawNode>, Spanned<RawNode>)>,
        seen: HashMap<String, Span>,
        event_count: usize,
        collection_depth: usize,
        result_bytes: usize,
    },
}

struct AnchoredNode {
    node: Spanned<RawNode>,
    event_count: usize,
    collection_depth: usize,
    result_bytes: usize,
}

struct Builder {
    file: Arc<str>,
    stack: Vec<Frame>,
    anchors: HashMap<usize, AnchoredNode>,
    traversed_nodes: usize,
    result_bytes: usize,
    discard_events: bool,
    /// One entry per YAML document seen (`None` for an empty document);
    /// checked against `== 1` once parsing finishes.
    documents: Vec<Option<Spanned<RawNode>>>,
    /// The most recently completed top-level (frame-less) node in the
    /// document currently being built.
    root: Option<Spanned<RawNode>>,
    /// The first structural error encountered (duplicate key, bad tag,
    /// non-scalar mapping key, …). Building continues after one is
    /// recorded so the event stream stays balanced, but `parse_raw` returns
    /// this instead of the tree once found.
    error: Option<ParseError>,
}
