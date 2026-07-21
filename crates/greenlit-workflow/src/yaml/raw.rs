//! The span-preserving raw YAML tree, and the event-driven builder that
//! produces it.
//!
//! This is the one module in the crate that touches `saphyr_parser` types
//! (design memo §6.3 risk mitigation: "confine all saphyr types to one
//! small internal module"). It drives the parser's low-level event stream
//! directly — rather than going through the higher-level `saphyr` crate's
//! `YamlLoader`/`MarkedYaml` — for one concrete reason: GitHub's workflow
//! YAML dialect rejects duplicate mapping keys outright and treats `<<` as
//! an ordinary (unsupported) key rather than performing a YAML-1.1 merge
//! (design memo §6.1), and `YamlLoader`'s internal map-building
//! (`hashlink::LinkedHashMap::insert`, which silently overwrites a
//! duplicate key) is not something a caller can hook or override — its
//! `doc_stack`/`key_stack` fields are private. Building the tree from raw
//! events instead gives full control over both checks. This is the same
//! "build the spanned tree from events" fallback the design memo describes
//! for `yaml-rust2`, applied to `saphyr-parser` (the actively-maintained
//! low-level half of the same project) for a correctness reason rather
//! than an availability one.
//!
//! Duplicate detection operates on each scalar key's decoded string value,
//! independent of whether YAML wrote it plain or quoted. This matches the
//! runner template reader, which converts a key to `StringToken` and adds
//! `nextKey.Value` to its duplicate set without carrying scalar style:
//! <https://github.com/actions/runner/blob/main/src/Sdk/DTObjectTemplating/ObjectTemplating/TemplateReader.cs>.

use crate::error::ParseError;
use crate::span::{Location, Span, Spanned};
use crate::yaml::scalar::YamlScalar;
use crate::yaml::tag::{resolve_core_tag, typed_scalar};
use saphyr_parser::{Event, Marker, Parser, ScalarStyle, SpannedEventReceiver};
use std::collections::HashMap;
use std::sync::Arc;

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
    Sequence(Vec<Spanned<RawNode>>),
    Mapping(Vec<(Spanned<RawNode>, Spanned<RawNode>)>),
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

/// One open sequence/mapping while walking the event stream.
enum Frame {
    Sequence {
        start: Marker,
        anchor_id: usize,
        items: Vec<Spanned<RawNode>>,
    },
    Mapping {
        start: Marker,
        anchor_id: usize,
        pending_key: Option<Spanned<RawNode>>,
        entries: Vec<(Spanned<RawNode>, Spanned<RawNode>)>,
        seen: HashMap<String, Span>,
    },
}

struct Builder {
    file: Arc<str>,
    stack: Vec<Frame>,
    anchors: HashMap<usize, Spanned<RawNode>>,
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

impl Builder {
    fn new(file: Arc<str>) -> Self {
        Self {
            file,
            stack: Vec::new(),
            anchors: HashMap::new(),
            documents: Vec::new(),
            root: None,
            error: None,
        }
    }

    fn record_error(&mut self, err: ParseError) {
        if self.error.is_none() {
            self.error = Some(err);
        }
    }

    fn finish_node(&mut self, node: Spanned<RawNode>, anchor_id: usize) {
        if anchor_id > 0 {
            self.anchors.insert(anchor_id, node.clone());
        }
        match self.stack.last_mut() {
            Some(Frame::Sequence { items, .. }) => items.push(node),
            Some(Frame::Mapping {
                pending_key,
                entries,
                seen,
                ..
            }) => match pending_key.take() {
                None => {
                    if node.value.as_key_text().is_none() {
                        // A direct field access (not `self.record_error(..)`,
                        // a whole-`&mut self` method call) so it can coexist
                        // with the active `self.stack.last_mut()` borrow —
                        // same reasoning as the `self.error.get_or_insert`
                        // call just below for duplicate keys.
                        self.error.get_or_insert(ParseError::Schema {
                            span: node.span.clone(),
                            message: "mapping keys must be scalars".to_owned(),
                        });
                    }
                    *pending_key = Some(node);
                }
                Some(key) => {
                    if let Some(key_text) = key.value.as_key_text() {
                        // `key_text` is saphyr's decoded scalar value; quote
                        // style lives separately on `RawScalar` and must not
                        // participate in identity (see module-level runner
                        // source citation).
                        let dedup_key = key_text.to_owned();
                        if let Some(first_span) = seen.get(&dedup_key) {
                            self.error.get_or_insert(ParseError::DuplicateKey {
                                span: key.span.clone(),
                                key: key_text.to_owned(),
                                first_span: first_span.clone(),
                            });
                        } else {
                            seen.insert(dedup_key, key.span.clone());
                        }
                    }
                    entries.push((key, node));
                }
            },
            None => self.root = Some(node),
        }
    }
}

impl<'input> SpannedEventReceiver<'input> for Builder {
    fn on_event(&mut self, ev: Event<'input>, span: saphyr_parser::Span) {
        let file = self.file.clone();
        let node_span = span_from(&file, span);
        match ev {
            Event::Nothing | Event::StreamStart | Event::StreamEnd => {}
            Event::DocumentStart(_) => {
                self.root = None;
            }
            Event::DocumentEnd => {
                let doc = self.root.take();
                self.documents.push(doc);
            }
            Event::Alias(id) => {
                let mut node = match self.anchors.get(&id) {
                    Some(n) => n.clone(),
                    None => {
                        // Unreachable in practice: `saphyr_parser::Parser`
                        // validates an alias against its registered anchors
                        // before ever emitting `Event::Alias` (see
                        // `Parser::next_event_impl`'s "found unknown anchor"
                        // scan error) — this branch only guards against that
                        // invariant being violated rather than panicking.
                        self.record_error(ParseError::Schema {
                            span: node_span.clone(),
                            message: "alias refers to an undefined anchor".to_owned(),
                        });
                        Spanned::new(
                            RawNode::Scalar(RawScalar {
                                raw: String::new(),
                                style: ScalarStyle::Plain,
                                value: YamlScalar::Null,
                            }),
                            node_span.clone(),
                        )
                    }
                };
                // Matches the behavior of `saphyr`'s own `YamlLoader`: the
                // alias use-site's span replaces the node's own (top-level)
                // span, but nested children keep the spans from the
                // anchor's original definition. See design memo §6.3 point
                // 3 (flagged there as an observed-behavior item for which
                // location GitHub's own errors would use).
                node.span = node_span;
                self.finish_node(node, 0);
            }
            Event::Scalar(text, style, anchor_id, tag) => {
                let tag_kind = resolve_core_tag(&tag, &node_span, &mut self.error);
                let value = typed_scalar(&text, style, tag_kind, &node_span, &mut self.error);
                let node = Spanned::new(
                    RawNode::Scalar(RawScalar {
                        raw: text.into_owned(),
                        style,
                        value,
                    }),
                    node_span,
                );
                self.finish_node(node, anchor_id);
            }
            Event::SequenceStart(anchor_id, tag) => {
                resolve_core_tag(&tag, &node_span, &mut self.error);
                self.stack.push(Frame::Sequence {
                    start: span.start,
                    anchor_id,
                    items: Vec::new(),
                });
            }
            Event::SequenceEnd => {
                if let Some(Frame::Sequence {
                    start,
                    anchor_id,
                    items,
                }) = self.stack.pop()
                {
                    let full_span = Span::new(
                        file.clone(),
                        location_from_marker(start),
                        location_from_marker(span.start),
                    );
                    self.finish_node(Spanned::new(RawNode::Sequence(items), full_span), anchor_id);
                }
            }
            Event::MappingStart(anchor_id, tag) => {
                resolve_core_tag(&tag, &node_span, &mut self.error);
                self.stack.push(Frame::Mapping {
                    start: span.start,
                    anchor_id,
                    pending_key: None,
                    entries: Vec::new(),
                    seen: HashMap::new(),
                });
            }
            Event::MappingEnd => {
                if let Some(Frame::Mapping {
                    start,
                    anchor_id,
                    entries,
                    ..
                }) = self.stack.pop()
                {
                    let full_span = Span::new(
                        file.clone(),
                        location_from_marker(start),
                        location_from_marker(span.start),
                    );
                    self.finish_node(
                        Spanned::new(RawNode::Mapping(entries), full_span),
                        anchor_id,
                    );
                }
            }
        }
    }
}
