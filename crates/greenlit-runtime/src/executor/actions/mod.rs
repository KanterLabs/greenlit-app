//! Executing `uses:` steps: the "Action execution" task group of
//! `PHASE-3-actions.md`, built on top of `greenlit-actions`' resolution/
//! fetch/manifest layer (the prior task group).
//!
//! # Module map
//!
//! (Every submodule below is crate-private — `crate::executor` is the
//! executor's own implementation, not this crate's public API — so they are
//! named here in plain text rather than as doc-links.)
//!
//! - `resolve` — the per-job pre-pass: turns every `uses:` step (recursing
//!   into composite actions up to GitHub's own documented nesting limit) into
//!   a `ResolvedUses` *before* the job container boots, so every
//!   read-only bind the job needs (fetched action source trees, pinned Node
//!   runtimes) is known at container-create time and so whether the job needs
//!   a Docker-action sibling container is known before the workspace
//!   isolation strategy is chosen (see `docker_action`'s module docs for
//!   why that matters).
//! - `checkout` — `actions/checkout` is special-cased entirely: self-
//!   checkout is satisfied from the already-isolated workspace with no
//!   network and no fetch of the real `actions/checkout` source at all;
//!   checkout of a different repository clones for real, inside the
//!   container, over the network.
//! - [`node_runtime`] — the pinned `actions/runner` Node runtime bundles
//!   (node20/node24, standard/Alpine), content-addressed cache, and the
//!   in-container libc probe that picks the Alpine variant.
//! - `nodejs` — JavaScript action execution: `INPUT_*`/`STATE_`/pre-post.
//! - `composite` — composite action execution: scoped `inputs`/`steps`/
//!   blocked `secrets`, nested `uses:`.
//! - `docker_action` — Docker action execution: build/pull through the
//!   engine trait, run as a sibling container.
//! - `post_chain` — the job-wide LIFO post-step stack every action kind
//!   (including ones nested inside a composite) registers onto.
//! - `template` — runtime `${{ }}` evaluation for manifest-sourced text
//!   (never plan-folded, unlike workflow YAML — see that module's docs).
//!
//! # Design note: why action execution needs a pre-pass at all
//!
//! Phase 2's step loop resolves everything context-sensitive *as it reaches
//! each step*, one at a time, because a `run:` step needs nothing but the
//! already-running job container. Action execution breaks that: a JS/
//! composite action's source must be *mounted into the container* to run at
//! all, a Docker action needs to know before the job container exists
//! whether it must share a writable workspace with a sibling (`docker_action`
//! module docs), and GitHub's own pre/post ordering ("pre steps run before
//! the first step of the job", `PHASE-3-actions.md`) requires knowing the
//! full list of an action's `pre:` scripts before the step loop starts at
//! all. `resolve::resolve_job_actions` is therefore run once per job,
//! before `super::job`'s container boot, and its output threads through
//! both container-spec assembly and the step loop.

pub(crate) mod checkout;
pub(crate) mod composite;
pub(crate) mod docker_action;
pub mod node_runtime;
pub(crate) mod nodejs;
pub(crate) mod post_chain;
pub(crate) mod resolve;
pub(crate) mod template;

use std::sync::Arc;

use greenlit_actions::resolve::RefResolver;
use greenlit_actions::store::{ActionFetcher, ActionStore};

use node_runtime::{NodeBundleSpecs, RuntimeBundleFetcher};

/// Resolves and fetches every statically materialized action in an execution
/// plan before the RunLock is finalized. Execution may then use a frozen
/// [`greenlit_actions::resolve::PinnedRefResolver`] without mutable aliases.
///
/// # Errors
/// Returns the same actionable resolution, fetch, manifest, or recursion
/// failure normal job preparation would return.
pub async fn preflight_plan_actions(
    plan: &greenlit_engine::ExecutionPlan,
    config: &ActionRuntimeConfig,
    repo_host_path: &std::path::Path,
    workspace: &str,
) -> Result<std::collections::BTreeMap<String, String>, crate::executor::ExecError> {
    let mut identities = std::collections::BTreeMap::new();
    for job in &plan.jobs {
        if !job.steps.is_empty() {
            let resolved =
                resolve::resolve_job_actions(&job.steps, config, repo_host_path, workspace).await?;
            identities.extend(resolved.identities);
        }
        for leg in &job.legs {
            let resolved =
                resolve::resolve_job_actions(&leg.steps, config, repo_host_path, workspace).await?;
            identities.extend(resolved.identities);
        }
    }
    Ok(identities)
}

/// Everything the executor needs to resolve, fetch, and run `uses:` steps,
/// injected once per `litci run` invocation.
///
/// Every trait-object field is a true external boundary (`TESTING.md`: "Mock
/// only true externals... the GitHub API"): production code
/// (`greenlit-app`'s `run_cmd`) wires the real GitHub API/tokenless resolver,
/// tarball/git-clone fetcher, and network Node-runtime downloader; tests
/// inject fakes so the executor's own orchestration is exercised without
/// touching the network, matching the "zero network fetches for pinned SHAs"
/// and "no live downloads in the default test path" invariants
/// (`PHASE-3-actions.md` exit criteria 4 and 6).
pub struct ActionRuntimeConfig {
    /// Resolves a `uses:` ref (tag/branch/SHA) to a commit SHA —
    /// [`greenlit_actions::resolve::GitHubApiResolver`] when a token is
    /// available, [`greenlit_actions::resolve::GitLsRemoteResolver`]
    /// otherwise (`greenlit-app`'s call, per `PHASE-3-actions.md`: "pick
    /// `GitHubApiResolver`(token) when Some, else `GitLsRemoteResolver`").
    pub resolver: Arc<dyn RefResolver>,
    /// The content-addressed action source store
    /// (`~/.litci/actions/<owner>/<repo>/<sha>/`).
    pub store: ActionStore,
    /// Fetches a resolved SHA's source into the store on a miss —
    /// typically [`greenlit_actions::store::FallbackFetcher`] wrapping a
    /// tarball-then-git-clone strategy.
    pub fetcher: Arc<dyn ActionFetcher>,
    /// Fetches/verifies a pinned Node runtime bundle (`node_runtime` module).
    pub node_runtime_fetcher: Arc<dyn RuntimeBundleFetcher>,
    /// Picks which bundle (URL, checksum) to ensure for a given Node
    /// version/libc variant — [`node_runtime::PinnedNodeBundleSpecs`] in
    /// production; a test injects a fake naming a tiny synthetic bundle's
    /// own checksum instead (`node_runtime::NodeBundleSpecs` module docs).
    pub node_runtime_specs: Arc<dyn NodeBundleSpecs>,
    /// Content-addressed cache of extracted Node runtime bundles
    /// (`~/.litci/node-runtimes/`), and its hit/miss tally.
    pub node_runtime_store: node_runtime::RuntimeStore,
    /// The token used for a real (non-self) `actions/checkout` clone, and
    /// for any registry/API call this task group makes on the user's
    /// behalf. `None` when `litci auth` was never run — a checkout of a
    /// different repository then fails fast with the `litci auth` fix
    /// (`PHASE-3-actions.md` "actions/checkout"), rather than attempting an
    /// anonymous clone that may or may not happen to work.
    pub github_token: Option<String>,
}
