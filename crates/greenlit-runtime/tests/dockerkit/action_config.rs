//! Production-shaped action configuration for quarantine-boundary tests.

use std::sync::Arc;

use greenlit_actions::resolve::GitLsRemoteResolver;
use greenlit_actions::store::{ActionStore, FallbackFetcher, GitCloneFetcher, TarballFetcher};
use greenlit_runtime::executor::actions::node_runtime::{
    HttpRuntimeBundleFetcher, PinnedNodeBundleSpecs, RuntimeStore,
};

/// Build real external-boundary implementations for a path whose quarantine
/// contract guarantees action resolution remains unreachable.
pub fn unreachable_action_config() -> greenlit_runtime::ActionRuntimeConfig {
    greenlit_runtime::ActionRuntimeConfig {
        resolver: Arc::new(GitLsRemoteResolver::new()),
        store: ActionStore::at(std::env::temp_dir().join("greenlit-test-unused-action-store")),
        fetcher: Arc::new(FallbackFetcher::new(
            TarballFetcher::new(),
            GitCloneFetcher::new(),
        )),
        node_runtime_fetcher: Arc::new(HttpRuntimeBundleFetcher::new()),
        node_runtime_specs: Arc::new(PinnedNodeBundleSpecs),
        node_runtime_store: RuntimeStore::at(
            std::env::temp_dir().join("greenlit-test-unused-node-store"),
        ),
        github_token: None,
    }
}
