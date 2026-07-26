//! The job-wide LIFO post-step stack.
//!
//! `PHASE-3-actions.md`: "post steps run in REVERSE order at job end
//! REGARDLESS of failure." Verified against the real runner: `ExecutionContext.
//! PostJobSteps` is a `Stack<IStep>` (`src/Runner.Worker/ExecutionContext.cs`,
//! `actions/runner` v2.336.0 — pinned release, see
//! `crate::executor::actions::node_runtime` module docs), pushed onto by
//! `RegisterPostJobStep` as each action's main step runs
//! (`src/Runner.Worker/ActionRunner.cs`) and drained with `TryPop` once every
//! job step has run (`src/Runner.Worker/StepsRunner.cs`) — a stack, so the
//! drain order is exactly the reverse of the push order. One flat stack
//! spans the whole job, including actions nested inside a composite
//! (`docs/adrs/1144-composite-actions.md`: "we will execute the pre and post
//! steps of any actions referenced in a composite action"), which is why
//! [`PostChain`] is threaded through composite execution
//! ([`super::composite`]) rather than each composite instance keeping its
//! own.
//!
//! Pushed the moment the main loop *reaches* a step with a post script,
//! before evaluating that step's own `if:` — GitHub schedules an action's
//! post unconditionally of whether its own main step ends up skipped; only
//! the post's own `post-if` (default `always()`, verified in
//! `src/Runner.Worker/ActionManifestManager.cs`) decides whether the post
//! body actually runs, evaluated separately at drain time.

use crate::executor::actions::checkout::CheckoutPost;

/// One JS action's post-execution request.
#[derive(Debug, Clone)]
pub(crate) struct NodePostEntry {
    /// Container path of the action's root directory.
    pub action_path: String,
    /// The `post:` script path, relative to `action_path`.
    pub post_script: String,
    /// Raw `post-if` text; `None` means the documented `always()` default.
    pub post_if: Option<String>,
    /// This instance's fully resolved `INPUT_*` environment, including
    /// action-manifest defaults. GitHub resolves inputs once per action
    /// instance and exposes the same values to main and post.
    pub input_env: Vec<(String, String)>,
    /// This step's own workflow-authored `env:` layer, resolved once at
    /// step-reach time (shared across pre/main/post, same as `with`).
    pub step_env: indexmap::IndexMap<String, String>,
    /// Which pinned Node runtime this action targets.
    pub node_version: greenlit_actions::manifest::NodeVersion,
    /// A stable key identifying this step instance's `STATE_` map in the
    /// job's `action_state` table (`crate::executor::step::StepLoopState`).
    pub state_key: usize,
}

/// One pending post-execution request, queued the moment its main step ran.
#[derive(Debug, Clone)]
pub(crate) enum PostAction {
    /// A JavaScript action's `post:` script.
    Node(Box<NodePostEntry>),
    /// `actions/checkout`'s (synthetic) post phase.
    Checkout(CheckoutPost),
}

/// One queued post action plus its display label.
#[derive(Debug, Clone)]
pub(crate) struct PostEntry {
    /// Display label, e.g. `"Post actions/checkout@v4"`.
    pub label: String,
    /// What to run.
    pub action: PostAction,
}

/// The job-wide LIFO post-step queue.
#[derive(Debug, Clone, Default)]
pub(crate) struct PostChain {
    entries: Vec<PostEntry>,
}

impl PostChain {
    /// Registers `entry`; drained in the reverse of push order.
    pub(crate) fn push(&mut self, entry: PostEntry) {
        self.entries.push(entry);
    }

    /// Takes every registered entry, in the reverse of the order they were
    /// pushed (LIFO), leaving the chain empty.
    pub(crate) fn drain_reverse(&mut self) -> Vec<PostEntry> {
        let mut entries = std::mem::take(&mut self.entries);
        entries.reverse();
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str) -> PostEntry {
        PostEntry {
            label: label.to_string(),
            action: PostAction::Checkout(CheckoutPost::Noop),
        }
    }

    #[test]
    fn drains_in_reverse_push_order() {
        let mut chain = PostChain::default();
        chain.push(entry("first"));
        chain.push(entry("second"));
        chain.push(entry("third"));
        let drained: Vec<String> = chain.drain_reverse().into_iter().map(|e| e.label).collect();
        assert_eq!(drained, vec!["third", "second", "first"]);
        assert!(chain.drain_reverse().is_empty(), "chain is consumed");
    }
}
