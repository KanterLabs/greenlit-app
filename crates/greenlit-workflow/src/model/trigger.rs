//! The `on:` key: all trigger forms.

use crate::model::value::{UnsupportedConstruct, YamlValue};
use crate::span::{Span, Spanned};

/// One entry of the workflow's `on:` trigger set, normalized from any of
/// the three YAML forms (`on: push`, `on: [push, pull_request]`,
/// `on: { push: …, pull_request: { … } }`) into a uniform list — see
/// `crate::parse::trigger` for the normalization.
#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    /// `push` / `pull_request` / `pull_request_target`.
    Webhook {
        /// The event name as written (`"push"`, `"pull_request"`, …).
        name: String,
        /// The event's branch/tag/path/type filters.
        filter: WebhookFilter,
    },
    /// `workflow_dispatch`.
    WorkflowDispatch(WorkflowDispatch),
    /// `schedule`.
    Schedule(Vec<Spanned<String>>),
    /// `repository_dispatch`.
    RepositoryDispatch {
        /// `types:` list; empty means "all types".
        types: Vec<Spanned<String>>,
    },
    /// `workflow_call` — recognized-but-rejected (reusable workflows are
    /// out of v0 scope; see `greenlit-v0-spec.md` "Out (v0)").
    WorkflowCall(UnsupportedConstruct),
    /// Any other named GitHub webhook event (`release`, `issues`,
    /// `workflow_run`, …) this crate does not model a dedicated shape for.
    /// Its configuration, if any, is preserved as a generic value rather
    /// than dropped.
    Other {
        /// The event name as written.
        name: String,
        /// The event's configuration mapping, if any.
        config: Option<Spanned<YamlValue>>,
    },
}

/// The branch/tag/path filters shared by `push`, `pull_request`, and
/// `pull_request_target`. An empty vector means "no filter on this axis"
/// (matches everything).
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookFilter {
    /// The complete event-filter mapping (or the event node itself when
    /// the trigger has no configuration mapping).
    pub span: Span,
    /// `branches:`.
    pub branches: Vec<Spanned<String>>,
    /// `branches-ignore:`.
    pub branches_ignore: Vec<Spanned<String>>,
    /// `tags:`.
    pub tags: Vec<Spanned<String>>,
    /// `tags-ignore:`.
    pub tags_ignore: Vec<Spanned<String>>,
    /// `paths:`.
    pub paths: Vec<Spanned<String>>,
    /// `paths-ignore:`.
    pub paths_ignore: Vec<Spanned<String>>,
    /// `types:` — only meaningful for `pull_request`/`pull_request_target`,
    /// but accepted generically.
    pub types: Vec<Spanned<String>>,
}

/// `workflow_dispatch:` configuration.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorkflowDispatch {
    /// `inputs:`, in file order.
    pub inputs: Vec<WorkflowDispatchInput>,
}

/// One `workflow_dispatch.inputs.<name>` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowDispatchInput {
    /// The complete `workflow_dispatch.inputs.<name>` configuration
    /// mapping.
    pub span: Span,
    /// The input's name (the `inputs:` mapping key).
    pub name: Spanned<String>,
    /// `description:`.
    pub description: Option<Spanned<String>>,
    /// `required:`.
    pub required: Option<Spanned<bool>>,
    /// `default:`.
    pub default: Option<Spanned<YamlValue>>,
    /// `type:`.
    pub input_type: Option<Spanned<WorkflowDispatchInputType>>,
    /// `options:` — only meaningful when `input_type` is `Choice`.
    pub options: Vec<Spanned<String>>,
}

/// `workflow_dispatch.inputs.<name>.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowDispatchInputType {
    /// `type: string`.
    String,
    /// `type: boolean`.
    Boolean,
    /// `type: number`.
    Number,
    /// `type: choice`.
    Choice,
    /// `type: environment`.
    Environment,
}
