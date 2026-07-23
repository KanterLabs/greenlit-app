//! Raw-tree builder state transitions, split from event dispatch.

use crate::error::ParseError;
use crate::span::{Span, Spanned};

use super::{
    AnchoredNode, Builder, Frame, MAX_TRAVERSED_YAML_NODES, MAX_YAML_RESULT_BYTES, RawNode,
};

impl Builder {
    pub(super) fn new(file: std::sync::Arc<str>) -> Self {
        Self {
            file,
            stack: Vec::new(),
            anchors: std::collections::HashMap::new(),
            traversed_nodes: 0,
            result_bytes: 0,
            discard_events: false,
            documents: Vec::new(),
            root: None,
            error: None,
        }
    }

    pub(super) fn record_error(&mut self, error: ParseError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    pub(super) fn reject_limit(&mut self, span: Span, message: String) {
        self.record_error(ParseError::YamlLimit { span, message });
        self.discard_events = true;
    }

    pub(super) fn count_events(&mut self, count: usize, span: &Span) -> bool {
        let next = self.traversed_nodes.saturating_add(count);
        if next > MAX_TRAVERSED_YAML_NODES {
            self.reject_limit(
                span.clone(),
                format!(
                    "workflow YAML exceeds GitHub's maximum of {MAX_TRAVERSED_YAML_NODES} traversed nodes after expanding aliases"
                ),
            );
            return false;
        }
        self.traversed_nodes = next;
        true
    }

    pub(super) fn count_result_bytes(&mut self, count: usize, span: &Span) -> bool {
        let next = self.result_bytes.saturating_add(count);
        if next > MAX_YAML_RESULT_BYTES {
            self.reject_limit(
                span.clone(),
                format!(
                    "workflow YAML exceeds GitHub's maximum parsed result size of {MAX_YAML_RESULT_BYTES} bytes after expanding aliases"
                ),
            );
            return false;
        }
        self.result_bytes = next;
        true
    }

    pub(super) fn finish_node(
        &mut self,
        node: Spanned<RawNode>,
        anchor_id: usize,
        event_count: usize,
        collection_depth: usize,
        result_bytes: usize,
    ) {
        if anchor_id > 0 {
            self.anchors.insert(
                anchor_id,
                AnchoredNode {
                    node: node.clone(),
                    event_count,
                    collection_depth,
                    result_bytes,
                },
            );
        }
        match self.stack.last_mut() {
            Some(Frame::Sequence {
                items,
                event_count: parent_events,
                collection_depth: parent_depth,
                result_bytes: parent_bytes,
                ..
            }) => {
                *parent_events = parent_events.saturating_add(event_count);
                *parent_depth = (*parent_depth).max(collection_depth);
                *parent_bytes = parent_bytes.saturating_add(result_bytes);
                items.push(node);
            }
            Some(Frame::Mapping {
                pending_key,
                entries,
                seen,
                event_count: parent_events,
                collection_depth: parent_depth,
                result_bytes: parent_bytes,
                ..
            }) => {
                *parent_events = parent_events.saturating_add(event_count);
                *parent_depth = (*parent_depth).max(collection_depth);
                *parent_bytes = parent_bytes.saturating_add(result_bytes);
                match pending_key.take() {
                    None => {
                        if node.value.as_key_text().is_none() {
                            self.error.get_or_insert(ParseError::Schema {
                                span: node.span.clone(),
                                message: "mapping keys must be scalars".to_owned(),
                            });
                        }
                        *pending_key = Some(node);
                    }
                    Some(key) => {
                        if let Some(key_text) = key.value.as_key_text() {
                            // Both fixed-schema and loose workflow mappings
                            // use StringComparer.OrdinalIgnoreCase in the
                            // runner's TemplateReader. This applies to job
                            // ids, env/matrix keys, and ordinary schema keys
                            // alike, after YAML decoding.
                            // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/ObjectTemplating/TemplateReader.cs#L166-L216
                            // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/ObjectTemplating/TemplateReader.cs#L302-L342
                            let dedup_key = greenlit_expr::value::ordinal_ignore_case_key(key_text);
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
                }
            }
            None => self.root = Some(node),
        }
    }
}
