//! A span-preserving raw YAML tree for `action.yml`, built by driving
//! `saphyr-parser`'s event stream directly — the same low-level approach
//! `greenlit-workflow` uses for workflow YAML and for the same reason:
//! GitHub's action manifest is parsed by the *identical*
//! `YamlObjectReader`/`TemplateReader` machinery as workflow YAML (both live
//! in `ActionManifestManager.Load`/`WorkflowManager.Load` in
//! `actions/runner`, both call `TemplateReader.Read` with a
//! `YamlObjectReader`), so the same duplicate-key rejection and node/depth/
//! result-size ceilings documented for `greenlit-workflow`'s tree builder
//! apply verbatim here. This is a second, independent implementation (not a
//! reuse of `greenlit-workflow::yaml::raw`) because that module is a
//! crate-private implementation detail, not part of `greenlit-workflow`'s
//! public API.
//!
//! # Deliberate simplification versus `greenlit-workflow`'s builder
//!
//! Real-world `action.yml` files do not use YAML anchors/aliases (there is
//! no idiomatic reason to — the schema has no field that benefits from
//! reusing a subtree), so this builder rejects them outright
//! ([`ManifestError::AnchorsNotSupported`]) rather than reproducing
//! `greenlit-workflow`'s `Arc`-shared alias-replay machinery. The event/
//! depth/result-size ceilings below still bound a pathological or hostile
//! manifest even without alias expansion to defend against.
//!
//! Explicit YAML tags (`!!str`, custom tags, …) are likewise never used in
//! practice and are rejected as an unsupported construct rather than
//! modeled.

use std::collections::HashMap;
use std::sync::Arc;

use greenlit_expr::value::ordinal_ignore_case_key;
use greenlit_workflow::{Location, Span, Spanned};
use saphyr_parser::{Event, Parser, ScalarStyle, SpannedEventReceiver};

use crate::manifest::ManifestError;

// Same runner-imposed ceilings `greenlit-workflow`'s tree builder cites,
// since `action.yml` is parsed by the identical `TemplateReader`/
// `YamlObjectReader` pair:
// https://github.com/actions/runner/blob/main/src/Sdk/WorkflowParser/Conversion/YamlObjectReader.cs
// https://github.com/actions/runner/blob/main/src/Sdk/WorkflowParser/ParseOptions.cs
const MAX_YAML_DEPTH: usize = 50;
const MAX_TRAVERSED_NODES: usize = 50_000;
const MAX_RESULT_BYTES: usize = 10 * 1024 * 1024;

/// A YAML scalar's raw text, its style, and whether it was written in a
/// quoted/block style (which pins it to a string regardless of its text —
/// see [`RawScalar::is_plain`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawScalar {
    pub(crate) raw: String,
    pub(crate) style: ScalarStyle,
}

impl RawScalar {
    /// Whether this scalar's *text* is eligible for GitHub's Null/Boolean
    /// matching — only unquoted (`Plain`) scalars are; explicit quoting or
    /// block style (`|`/`>`) already pins the value to a string.
    pub(crate) fn is_plain(&self) -> bool {
        matches!(self.style, ScalarStyle::Plain)
    }
}

/// A YAML node with structure preserved but no manifest-schema
/// interpretation yet (that is `crate::manifest::parse`'s job).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RawNode {
    Scalar(RawScalar),
    Sequence(Vec<Spanned<RawNode>>),
    Mapping(Vec<(Spanned<RawNode>, Spanned<RawNode>)>),
}

impl RawNode {
    /// The scalar's raw source text, for key-like uses that must ignore
    /// scalar typing entirely (mapping keys, input/output ids). `None` for
    /// non-scalar nodes.
    pub(crate) fn as_key_text(&self) -> Option<&str> {
        match self {
            RawNode::Scalar(s) => Some(s.raw.as_str()),
            RawNode::Sequence(_) | RawNode::Mapping(_) => None,
        }
    }
}

/// Parses `source` (the contents of `file`, an `action.yml`/`action.yaml`
/// path used only for error messages) into a single raw YAML document.
///
/// # Errors
/// [`ManifestError::Yaml`] for malformed YAML syntax,
/// [`ManifestError::MultipleDocuments`]/[`ManifestError::EmptyDocument`] for
/// a file that is not exactly one document, and the structural variants
/// ([`ManifestError::DuplicateKey`], [`ManifestError::AnchorsNotSupported`],
/// [`ManifestError::YamlLimit`], [`ManifestError::NonScalarKey`]) raised
/// while building the tree.
pub(crate) fn parse_raw(file: Arc<str>, source: &str) -> Result<Spanned<RawNode>, ManifestError> {
    let mut parser = Parser::new_from_str(source);
    let mut builder = Builder::new(file.clone());
    parser
        .load(&mut builder, true)
        .map_err(|scan_error| yaml_syntax_error(&file, &scan_error))?;
    if let Some(error) = builder.error {
        return Err(error);
    }
    match builder.documents.len() {
        1 => builder
            .documents
            .into_iter()
            .next()
            .flatten()
            .ok_or(ManifestError::EmptyDocument { path: file }),
        0 => Err(ManifestError::EmptyDocument { path: file }),
        count => Err(ManifestError::MultipleDocuments { path: file, count }),
    }
}

fn yaml_syntax_error(file: &Arc<str>, error: &saphyr_parser::ScanError) -> ManifestError {
    let marker = error.marker();
    ManifestError::Yaml {
        path: file.clone(),
        // saphyr-parser's `Marker::col()` is 0-based despite its own doc
        // comment; see `greenlit_workflow`'s identical normalization note.
        line: u32::try_from(marker.line()).unwrap_or(u32::MAX),
        column: u32::try_from(marker.col())
            .unwrap_or(u32::MAX)
            .saturating_add(1),
        message: error.info().to_string(),
    }
}

fn location_from_marker(marker: saphyr_parser::Marker) -> Location {
    Location::new(
        u32::try_from(marker.line()).unwrap_or(u32::MAX),
        u32::try_from(marker.col())
            .unwrap_or(u32::MAX)
            .saturating_add(1),
    )
}

fn span_from(file: &Arc<str>, span: saphyr_parser::Span) -> Span {
    Span::new(
        file.clone(),
        location_from_marker(span.start),
        location_from_marker(span.end),
    )
}

enum Frame {
    Sequence {
        start: saphyr_parser::Marker,
        items: Vec<Spanned<RawNode>>,
    },
    Mapping {
        start: saphyr_parser::Marker,
        pending_key: Option<Spanned<RawNode>>,
        entries: Vec<(Spanned<RawNode>, Spanned<RawNode>)>,
        seen: HashMap<String, Span>,
    },
}

struct Builder {
    file: Arc<str>,
    stack: Vec<Frame>,
    traversed_nodes: usize,
    result_bytes: usize,
    discard_events: bool,
    documents: Vec<Option<Spanned<RawNode>>>,
    root: Option<Spanned<RawNode>>,
    error: Option<ManifestError>,
}

impl Builder {
    fn new(file: Arc<str>) -> Self {
        Self {
            file,
            stack: Vec::new(),
            traversed_nodes: 0,
            result_bytes: 0,
            discard_events: false,
            documents: Vec::new(),
            root: None,
            error: None,
        }
    }

    fn record_error(&mut self, error: ManifestError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn reject_limit(&mut self, span: Span, message: String) {
        self.record_error(ManifestError::YamlLimit { span, message });
        self.discard_events = true;
    }

    fn count_node(&mut self, span: &Span) -> bool {
        self.traversed_nodes = self.traversed_nodes.saturating_add(1);
        if self.traversed_nodes > MAX_TRAVERSED_NODES {
            self.reject_limit(
                span.clone(),
                format!("action.yml exceeds the maximum of {MAX_TRAVERSED_NODES} traversed nodes"),
            );
            return false;
        }
        true
    }

    fn count_bytes(&mut self, bytes: usize, span: &Span) -> bool {
        self.result_bytes = self.result_bytes.saturating_add(bytes);
        if self.result_bytes > MAX_RESULT_BYTES {
            self.reject_limit(
                span.clone(),
                format!(
                    "action.yml exceeds the maximum parsed result size of {MAX_RESULT_BYTES} bytes"
                ),
            );
            return false;
        }
        true
    }

    fn finish_node(&mut self, node: Spanned<RawNode>) {
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
                        self.error.get_or_insert(ManifestError::NonScalarKey {
                            span: node.span.clone(),
                        });
                    }
                    *pending_key = Some(node);
                }
                Some(key) => {
                    if let Some(key_text) = key.value.as_key_text() {
                        // Same `StringComparer.OrdinalIgnoreCase` comparison
                        // the runner's `TemplateReader` applies to every
                        // mapping key it reads, for both workflow YAML and
                        // `action.yml` (see module docs).
                        let dedup_key = ordinal_ignore_case_key(key_text);
                        if let Some(first_span) = seen.get(&dedup_key) {
                            self.error.get_or_insert(ManifestError::DuplicateKey {
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
    fn on_event(&mut self, event: Event<'input>, span: saphyr_parser::Span) {
        if self.discard_events {
            return;
        }
        let file = self.file.clone();
        let node_span = span_from(&file, span);
        if !matches!(event, Event::Nothing) && !self.count_node(&node_span) {
            return;
        }
        match event {
            Event::Nothing | Event::StreamStart | Event::StreamEnd => {}
            Event::DocumentStart(_) => self.root = None,
            Event::DocumentEnd => {
                let document = self.root.take();
                self.documents.push(document);
            }
            Event::Alias(_) => {
                self.record_error(ManifestError::AnchorsNotSupported { span: node_span });
            }
            Event::Scalar(text, style, anchor_id, tag) => {
                if anchor_id > 0 {
                    self.record_error(ManifestError::AnchorsNotSupported {
                        span: node_span.clone(),
                    });
                }
                if tag.is_some() {
                    self.record_error(ManifestError::ExplicitTagsNotSupported {
                        span: node_span.clone(),
                    });
                }
                let byte_len = text.len().saturating_add(32);
                if !self.count_bytes(byte_len, &node_span) {
                    return;
                }
                self.finish_node(Spanned::new(
                    RawNode::Scalar(RawScalar {
                        raw: text.into_owned(),
                        style,
                    }),
                    node_span,
                ));
            }
            Event::SequenceStart(anchor_id, _tag) => {
                // Collection tags are never inspected: the pinned runner
                // ignores every tag attached to a sequence or mapping (see
                // `greenlit_workflow::yaml::raw`'s identical comment on its
                // own `SequenceStart`/`MappingStart` handling).
                if anchor_id > 0 {
                    self.record_error(ManifestError::AnchorsNotSupported {
                        span: node_span.clone(),
                    });
                }
                if self.stack.len() >= MAX_YAML_DEPTH {
                    self.reject_limit(
                        node_span,
                        format!(
                            "action.yml exceeds the maximum collection depth of {MAX_YAML_DEPTH}"
                        ),
                    );
                    return;
                }
                self.stack.push(Frame::Sequence {
                    start: span.start,
                    items: Vec::new(),
                });
            }
            Event::SequenceEnd => {
                if let Some(Frame::Sequence { start, items }) = self.stack.pop() {
                    let full_span = Span::new(
                        file,
                        location_from_marker(start),
                        location_from_marker(span.start),
                    );
                    self.finish_node(Spanned::new(RawNode::Sequence(items), full_span));
                }
            }
            Event::MappingStart(anchor_id, _tag) => {
                if anchor_id > 0 {
                    self.record_error(ManifestError::AnchorsNotSupported {
                        span: node_span.clone(),
                    });
                }
                if self.stack.len() >= MAX_YAML_DEPTH {
                    self.reject_limit(
                        node_span,
                        format!(
                            "action.yml exceeds the maximum collection depth of {MAX_YAML_DEPTH}"
                        ),
                    );
                    return;
                }
                self.stack.push(Frame::Mapping {
                    start: span.start,
                    pending_key: None,
                    entries: Vec::new(),
                    seen: HashMap::new(),
                });
            }
            Event::MappingEnd => {
                if let Some(Frame::Mapping { start, entries, .. }) = self.stack.pop() {
                    let full_span = Span::new(
                        file,
                        location_from_marker(start),
                        location_from_marker(span.start),
                    );
                    self.finish_node(Spanned::new(RawNode::Mapping(entries), full_span));
                }
            }
        }
    }
}
