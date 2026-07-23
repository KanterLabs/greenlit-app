//! Dispatches `saphyr-parser` events into the raw-tree builder state.

use std::collections::HashMap;

use saphyr_parser::{Event, ScalarStyle, SpannedEventReceiver};

use crate::error::ParseError;
use crate::span::{Span, Spanned};
use crate::yaml::scalar::YamlScalar;
use crate::yaml::tag::{resolve_scalar_core_tag, typed_scalar};

use super::{
    Builder, Frame, MAX_YAML_DEPTH, MIN_TOKEN_BYTES, RawNode, RawScalar, location_from_marker,
    scalar_result_bytes, span_from,
};

impl<'input> SpannedEventReceiver<'input> for Builder {
    fn on_event(&mut self, event: Event<'input>, span: saphyr_parser::Span) {
        if self.discard_events {
            return;
        }
        let file = self.file.clone();
        let node_span = span_from(&file, span);
        if !matches!(event, Event::Nothing) && !self.count_events(1, &node_span) {
            return;
        }
        match event {
            Event::Nothing | Event::StreamStart | Event::StreamEnd => {}
            Event::DocumentStart(_) => {
                self.root = None;
            }
            Event::DocumentEnd => {
                let document = self.root.take();
                self.documents.push(document);
            }
            Event::Alias(id) => {
                let (event_count, collection_depth, result_bytes) = match self.anchors.get(&id) {
                    Some(anchored) => (
                        anchored.event_count,
                        anchored.collection_depth,
                        anchored.result_bytes,
                    ),
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
                        let node = Spanned::new(
                            RawNode::Scalar(RawScalar {
                                raw: String::new(),
                                style: ScalarStyle::Plain,
                                value: YamlScalar::Null,
                            }),
                            node_span.clone(),
                        );
                        if self.count_result_bytes(MIN_TOKEN_BYTES, &node_span) {
                            self.finish_node(node, 0, 1, 0, MIN_TOKEN_BYTES);
                        }
                        return;
                    }
                };
                if self.stack.len().saturating_add(collection_depth) > MAX_YAML_DEPTH {
                    self.reject_limit(
                        node_span,
                        format!(
                            "workflow YAML exceeds GitHub's maximum collection depth of {MAX_YAML_DEPTH} after expanding an alias"
                        ),
                    );
                    return;
                }
                if !self.count_events(event_count.saturating_sub(1), &node_span) {
                    return;
                }
                if !self.count_result_bytes(result_bytes, &node_span) {
                    return;
                }
                // Do not clone/materialize the alias until all runner-equivalent
                // budgets have accepted it. `RawNode` collection clones are
                // shallow through their private `Arc` children, so nested
                // anchors do not retain another deep copy of the same subtree.
                let Some(anchored) = self.anchors.get(&id) else {
                    return;
                };
                let mut node = anchored.node.clone();
                // Matches the behavior of `saphyr`'s own `YamlLoader`: the
                // alias use-site's span replaces the node's own (top-level)
                // span, but nested children keep the spans from the
                // anchor's original definition. This follows `saphyr`'s
                // own `YamlLoader` alias cloning behavior; the public span
                // contract is exercised through `parse_workflow`.
                node.span = node_span;
                self.finish_node(node, 0, event_count, collection_depth, result_bytes);
            }
            Event::Scalar(text, style, anchor_id, tag) => {
                let tag_kind = resolve_scalar_core_tag(&tag, &node_span, &mut self.error);
                let value = typed_scalar(&text, style, tag_kind, &node_span, &mut self.error);
                let node = Spanned::new(
                    RawNode::Scalar(RawScalar {
                        raw: text.into_owned(),
                        style,
                        value,
                    }),
                    node_span,
                );
                let result_bytes = scalar_result_bytes(&node.value);
                if !self.count_result_bytes(result_bytes, &node.span) {
                    return;
                }
                self.finish_node(node, anchor_id, 1, 0, result_bytes);
            }
            Event::SequenceStart(anchor_id, _tag) => {
                // The pinned runner dispatches collection starts solely by
                // event type and never inspects their YAML tag. Therefore
                // even a custom or mismatched tag leaves the collection's
                // actual sequence shape unchanged.
                // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/Conversion/YamlObjectReader.cs#L114-L149
                if self.stack.len() >= MAX_YAML_DEPTH {
                    self.reject_limit(
                        node_span,
                        format!(
                            "workflow YAML exceeds GitHub's maximum collection depth of {MAX_YAML_DEPTH}"
                        ),
                    );
                    return;
                }
                if !self.count_result_bytes(MIN_TOKEN_BYTES, &node_span) {
                    return;
                }
                self.stack.push(Frame::Sequence {
                    start: span.start,
                    anchor_id,
                    items: Vec::new(),
                    event_count: 1,
                    collection_depth: 0,
                    result_bytes: MIN_TOKEN_BYTES,
                });
            }
            Event::SequenceEnd => {
                if let Some(Frame::Sequence {
                    start,
                    anchor_id,
                    items,
                    event_count,
                    collection_depth,
                    result_bytes,
                }) = self.stack.pop()
                {
                    let full_span = Span::new(
                        file.clone(),
                        location_from_marker(start),
                        location_from_marker(span.start),
                    );
                    self.finish_node(
                        Spanned::new(RawNode::Sequence(items.into()), full_span),
                        anchor_id,
                        event_count.saturating_add(1),
                        collection_depth.saturating_add(1),
                        result_bytes,
                    );
                }
            }
            Event::MappingStart(anchor_id, _tag) => {
                // See the sequence-start branch above: collection tags are
                // intentionally ignored by the runner.
                if self.stack.len() >= MAX_YAML_DEPTH {
                    self.reject_limit(
                        node_span,
                        format!(
                            "workflow YAML exceeds GitHub's maximum collection depth of {MAX_YAML_DEPTH}"
                        ),
                    );
                    return;
                }
                if !self.count_result_bytes(MIN_TOKEN_BYTES, &node_span) {
                    return;
                }
                self.stack.push(Frame::Mapping {
                    start: span.start,
                    anchor_id,
                    pending_key: None,
                    entries: Vec::new(),
                    seen: HashMap::new(),
                    event_count: 1,
                    collection_depth: 0,
                    result_bytes: MIN_TOKEN_BYTES,
                });
            }
            Event::MappingEnd => {
                if let Some(Frame::Mapping {
                    start,
                    anchor_id,
                    entries,
                    event_count,
                    collection_depth,
                    result_bytes,
                    ..
                }) = self.stack.pop()
                {
                    let full_span = Span::new(
                        file.clone(),
                        location_from_marker(start),
                        location_from_marker(span.start),
                    );
                    self.finish_node(
                        Spanned::new(RawNode::Mapping(entries.into()), full_span),
                        anchor_id,
                        event_count.saturating_add(1),
                        collection_depth.saturating_add(1),
                        result_bytes,
                    );
                }
            }
        }
    }
}
