//! The per-job action-resolution pre-pass: turns every `uses:` step
//! (recursing into composite actions) into a [`ResolvedUses`] *before* the
//! job container boots.
//!
//! See the parent module's docs for why this runs ahead of
//! [`super::super::job::boot_container`] rather than lazily, step by step,
//! the way `run:` steps are handled.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use greenlit_actions::manifest::{ActionInput, ActionManifest, ActionOutput, CompositeStep, Runs};
use greenlit_actions::resolve::resolve_ref;
use greenlit_actions::{ActionRef, RepositoryRef};
use greenlit_engine::StepKind;
use greenlit_workflow::Span;

use crate::engine::BindMount;
use crate::executor::ExecError;

use super::ActionRuntimeConfig;

/// GitHub's own documented composite-action recursion limit
/// (`docs/adrs/1144-composite-actions.md`, `actions/runner` v2.336.0 —
/// pinned release, see `super::node_runtime` module docs): "We will start
/// with supporting a recursion limit of `10` composite actions deep." A
/// workflow step itself is depth `0`; a composite it references is depth
/// `1`, and so on.
pub(crate) const MAX_COMPOSITE_DEPTH: u32 = 10;

/// In-container root every fetched (non-local) action's source is bound
/// read-only under, mirroring the host store's own
/// `<owner>/<repo>/<sha>` layout so container paths are predictable and
/// never collide across actions.
const ACTIONS_MOUNT_ROOT: &str = "/greenlit/actions";

/// One resolved `uses:` step, ready to dispatch to its execution module.
#[derive(Debug, Clone)]
pub(crate) enum ResolvedUses {
    /// `actions/checkout` — special-cased entirely, see
    /// `crate::executor::actions::checkout`.
    Checkout,
    /// A JavaScript action.
    Node(ResolvedNode),
    /// A composite action.
    Composite(ResolvedComposite),
    /// A Docker action (`docker://...` or a repository action with
    /// `runs.using: docker`).
    Docker(ResolvedDocker),
}

/// A resolved JavaScript action.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedNode {
    /// In-container path of the action's root directory (where `main`/
    /// `pre`/`post` are relative to).
    pub action_path: String,
    /// The action's declared inputs (for default-filling).
    pub inputs: IndexMap<String, ActionInput>,
    /// The action's `runs:` block.
    pub runs: greenlit_actions::manifest::NodeRuns,
}

/// A resolved composite action.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedComposite {
    /// In-container path of the action's root directory.
    pub action_path: String,
    /// The action's declared inputs (for default-filling and the nested
    /// `inputs` context).
    pub inputs: IndexMap<String, ActionInput>,
    /// The action's declared outputs (composite `outputs.<id>.value`).
    pub outputs: IndexMap<String, ActionOutput>,
    /// The composite's own steps, each with its `uses:` (if any) already
    /// resolved.
    pub steps: Vec<ResolvedCompositeStep>,
}

/// One composite step, its nested `uses:` (if any) already resolved.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedCompositeStep {
    /// The original step, verbatim (its own `run`/`uses`/`with`/`env`/`if`
    /// text, still to be evaluated at execution time).
    pub source: CompositeStep,
    /// The resolution of `source.uses`, when present.
    pub uses: Option<Box<ResolvedUses>>,
}

/// Where a Docker action's image comes from.
#[derive(Debug, Clone)]
pub(crate) enum DockerImageSource {
    /// Build from a `Dockerfile` inside the action's source, addressed by
    /// its host path (the build context is assembled host-side by reading
    /// the action's already-fetched files directly — no container bind is
    /// needed for a Docker build, unlike JS/composite execution).
    Build {
        /// Host directory containing the Dockerfile (the action's root).
        host_action_dir: PathBuf,
        /// The Dockerfile's path, relative to `host_action_dir`.
        dockerfile: String,
    },
    /// Pull a published image reference.
    Pull {
        /// The image reference, verbatim (may still contain `${{ }}`).
        image: String,
    },
}

/// A resolved Docker action.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedDocker {
    /// Where the image comes from.
    pub source: DockerImageSource,
    /// The action's declared inputs (for `INPUT_*` default-filling — Docker
    /// actions receive inputs exactly like JS actions,
    /// `INPUT_<NAME>` env vars, not a `${{ inputs.* }}` context, which only
    /// composite actions have). Empty for a bare `docker://image` step
    /// (no manifest, so no declared inputs at all).
    pub inputs: IndexMap<String, ActionInput>,
    /// `runs.args`, verbatim (resolved against context at execution time).
    pub args: Vec<String>,
    /// `runs.entrypoint`, verbatim, if declared.
    pub entrypoint: Option<String>,
    /// `runs.env`, verbatim (resolved against context at execution time).
    pub env: IndexMap<String, String>,
}

/// Everything the pre-pass learned about one job's `uses:` steps, aligned by
/// step index with the job's own step list.
#[derive(Debug, Clone, Default)]
pub(crate) struct JobActionPlan {
    /// `Some` at index `i` iff `steps[i]` is a `uses:` step.
    pub per_step: Vec<Option<ResolvedUses>>,
    /// Read-only binds the job container needs for every fetched action's
    /// source tree, deduplicated by host directory.
    pub binds: Vec<BindMount>,
    /// Whether any resolved action (at any nesting depth) runs via Docker —
    /// decides the job's workspace-sharing strategy (`super::docker_action`
    /// module docs).
    pub needs_docker_sibling: bool,
    /// Whether any resolved action needs the pinned Node 20 runtime.
    pub needs_node20: bool,
    /// Whether any resolved action needs the pinned Node 24 runtime.
    pub needs_node24: bool,
    /// Every repository action reference mapped to its resolved commit.
    pub identities: BTreeMap<String, String>,
    /// Static registry images required by Docker actions at any nesting depth.
    pub container_images: std::collections::BTreeSet<String>,
}

/// Accumulates the aggregate results (binds, flags) while [`resolve_uses`]
/// recurses. Deliberately lifetime-free (unlike a naive "just borrow
/// everything" accumulator would be): `resolve_uses` recurses through an
/// explicitly boxed, self-referential `Future`, and giving this type a
/// borrowed field would force every recursive call's `Future` to share one
/// exact lifetime with the borrowed config/workspace path, which the
/// recursive-`async fn` boxing pattern cannot express cleanly. `config` and
/// `repo_host_path` are threaded through as ordinary short-lived parameters
/// instead.
#[derive(Default)]
struct Collector {
    binds: Vec<BindMount>,
    seen_dirs: HashSet<String>,
    needs_docker_sibling: bool,
    needs_node20: bool,
    needs_node24: bool,
    identities: BTreeMap<String, String>,
    container_images: std::collections::BTreeSet<String>,
}

/// Resolves every `uses:` step in `steps` (recursing into composites),
/// producing the job-wide [`JobActionPlan`].
///
/// # Errors
/// Returns [`ExecError`] on a malformed `uses:` reference, a ref-resolution
/// or fetch failure, a manifest parse failure, or exceeding
/// [`MAX_COMPOSITE_DEPTH`].
pub(crate) async fn resolve_job_actions(
    steps: &[greenlit_engine::StepPlan],
    config: &ActionRuntimeConfig,
    repo_host_path: &Path,
    workspace: &str,
) -> Result<JobActionPlan, ExecError> {
    let env = ResolveEnv {
        config,
        repo_host_path,
        workspace,
    };
    let mut collector = Collector::default();
    let mut per_step = Vec::with_capacity(steps.len());
    for step in steps {
        let statically_skipped = step.condition.as_ref().is_some_and(|condition| {
            matches!(condition.eval, greenlit_engine::PlannedCond::Static(false))
        });
        let resolved = if statically_skipped {
            None
        } else {
            match &step.kind {
                StepKind::Run { .. } => None,
                StepKind::Uses {
                    reference, span, ..
                } => Some(resolve_uses(reference, span, 0, &env, &mut collector).await?),
            }
        };
        per_step.push(resolved);
    }
    Ok(JobActionPlan {
        per_step,
        binds: collector.binds,
        needs_docker_sibling: collector.needs_docker_sibling,
        needs_node20: collector.needs_node20,
        needs_node24: collector.needs_node24,
        identities: collector.identities,
        container_images: collector.container_images,
    })
}

/// The parts of a resolution that never change across one job's whole
/// recursive resolve (only `reference`/`span`/`depth` do), bundled so
/// [`resolve_uses`]/[`dispatch_manifest`] each take one parameter for them
/// instead of three.
struct ResolveEnv<'a> {
    config: &'a ActionRuntimeConfig,
    repo_host_path: &'a Path,
    workspace: &'a str,
}

/// One resolved action manifest plus the concrete paths it was resolved to,
/// bundled so [`dispatch_manifest`] takes one parameter for them instead of
/// three.
struct ManifestResolution {
    manifest: ActionManifest,
    /// In-container path of the action's root directory.
    container_path: String,
    /// Host path of the action's root directory (for a Docker build's
    /// context — see [`DockerImageSource::Build`]).
    host_action_dir: PathBuf,
}

fn resolve_uses<'a>(
    reference: &'a str,
    span: &'a Span,
    depth: u32,
    env: &'a ResolveEnv<'a>,
    collector: &'a mut Collector,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ResolvedUses, ExecError>> + Send + 'a>>
{
    Box::pin(async move {
        if depth > MAX_COMPOSITE_DEPTH {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "'{reference}' exceeds GitHub's {MAX_COMPOSITE_DEPTH}-level composite action nesting limit"
                ),
                fix: "flatten the nested composite actions".to_string(),
            });
        }
        let parsed = ActionRef::parse(reference).map_err(|source| ExecError::ActionRefInvalid {
            reference: reference.to_string(),
            span: span.clone(),
            source,
        })?;
        match parsed {
            ActionRef::Docker { image } => {
                collector.needs_docker_sibling = true;
                collector.container_images.insert(image.clone());
                Ok(ResolvedUses::Docker(ResolvedDocker {
                    source: DockerImageSource::Pull { image },
                    inputs: IndexMap::new(),
                    args: Vec::new(),
                    entrypoint: None,
                    env: IndexMap::new(),
                }))
            }
            ActionRef::Local { path } => {
                let host_dir = env.repo_host_path.join(path.trim_start_matches("./"));
                let manifest =
                    greenlit_actions::manifest::load_manifest(&host_dir).map_err(|source| {
                        ExecError::ActionManifest {
                            reference: reference.to_string(),
                            span: span.clone(),
                            source: Box::new(source),
                        }
                    })?;
                // Local actions live inside the workspace's own repo bind
                // already — no extra bind mount needed.
                let container_path = format!(
                    "{}/{}",
                    env.workspace.trim_end_matches('/'),
                    path.trim_start_matches("./")
                );
                dispatch_manifest(
                    reference,
                    span,
                    depth,
                    env,
                    collector,
                    ManifestResolution {
                        manifest,
                        container_path,
                        host_action_dir: host_dir,
                    },
                )
                .await
            }
            ActionRef::Repository(RepositoryRef {
                owner,
                repo,
                path,
                git_ref,
            }) => {
                if owner == "actions" && repo == "checkout" {
                    let sha = resolve_ref(env.config.resolver.as_ref(), &owner, &repo, &git_ref)
                        .await
                        .map_err(|source| ExecError::ActionResolve {
                            reference: reference.to_string(),
                            span: span.clone(),
                            source: Box::new(source),
                        })?;
                    collector
                        .identities
                        .insert(reference.to_string(), sha.as_str().to_string());
                    return Ok(ResolvedUses::Checkout);
                }
                let sha = resolve_ref(env.config.resolver.as_ref(), &owner, &repo, &git_ref)
                    .await
                    .map_err(|source| ExecError::ActionResolve {
                        reference: reference.to_string(),
                        span: span.clone(),
                        source: Box::new(source),
                    })?;
                collector
                    .identities
                    .insert(reference.to_string(), sha.as_str().to_string());
                let (host_dir, _outcome) = env
                    .config
                    .store
                    .ensure_fetched(&owner, &repo, &sha, env.config.fetcher.as_ref())
                    .await
                    .map_err(|source| ExecError::ActionFetch {
                        reference: reference.to_string(),
                        span: span.clone(),
                        source: Box::new(source),
                    })?;
                let manifest_host_dir = match &path {
                    Some(sub) => host_dir.join(sub),
                    None => host_dir.clone(),
                };
                let manifest = greenlit_actions::manifest::load_manifest(&manifest_host_dir)
                    .map_err(|source| ExecError::ActionManifest {
                        reference: reference.to_string(),
                        span: span.clone(),
                        source: Box::new(source),
                    })?;
                let container_root = mount_path(&owner, &repo, &sha);
                register_bind(collector, &host_dir, &container_root);
                let container_path = match &path {
                    Some(sub) => format!("{container_root}/{sub}"),
                    None => container_root,
                };
                dispatch_manifest(
                    reference,
                    span,
                    depth,
                    env,
                    collector,
                    ManifestResolution {
                        manifest,
                        container_path,
                        host_action_dir: manifest_host_dir,
                    },
                )
                .await
            }
        }
    })
}

async fn dispatch_manifest(
    reference: &str,
    span: &Span,
    depth: u32,
    env: &ResolveEnv<'_>,
    collector: &mut Collector,
    resolution: ManifestResolution,
) -> Result<ResolvedUses, ExecError> {
    let ManifestResolution {
        manifest,
        container_path,
        host_action_dir,
    } = resolution;
    match manifest.runs {
        Runs::Node(runs) => {
            match runs.using {
                greenlit_actions::manifest::NodeVersion::Node20 => collector.needs_node20 = true,
                greenlit_actions::manifest::NodeVersion::Node24 => collector.needs_node24 = true,
            }
            Ok(ResolvedUses::Node(ResolvedNode {
                action_path: container_path,
                inputs: manifest.inputs,
                runs,
            }))
        }
        Runs::Composite { steps } => {
            let mut resolved_steps = Vec::with_capacity(steps.len());
            for step in steps {
                let uses = match &step.uses {
                    Some(nested_ref) => {
                        // Nested `uses:` inside a composite carries no
                        // separate span of its own in this crate's model
                        // (`CompositeStep` is not `Spanned`); the enclosing
                        // step's span is reused for any resulting error.
                        Some(Box::new(
                            resolve_uses(nested_ref, span, depth + 1, env, collector).await?,
                        ))
                    }
                    None => None,
                };
                resolved_steps.push(ResolvedCompositeStep { source: step, uses });
            }
            Ok(ResolvedUses::Composite(ResolvedComposite {
                action_path: container_path,
                inputs: manifest.inputs,
                outputs: manifest.outputs,
                steps: resolved_steps,
            }))
        }
        Runs::Docker(docker_runs) => {
            collector.needs_docker_sibling = true;
            let source = match docker_runs.image.strip_prefix("docker://") {
                Some(image) => {
                    if image.contains("${{") {
                        return Err(ExecError::Infrastructure {
                            message: format!(
                                "Docker action '{reference}' has a runtime-dependent registry image"
                            ),
                            fix: "pin the action to a Dockerfile or a statically resolvable docker:// image"
                                .to_string(),
                        });
                    }
                    collector.container_images.insert(image.to_string());
                    DockerImageSource::Pull {
                        image: image.to_string(),
                    }
                }
                None => DockerImageSource::Build {
                    host_action_dir,
                    dockerfile: docker_runs.image,
                },
            };
            let _ = (reference, span, depth, container_path);
            Ok(ResolvedUses::Docker(ResolvedDocker {
                source,
                inputs: manifest.inputs,
                args: docker_runs.args,
                entrypoint: docker_runs.entrypoint,
                env: docker_runs.env.into_iter().collect(),
            }))
        }
    }
}

fn mount_path(owner: &str, repo: &str, sha: &greenlit_actions::CommitSha) -> String {
    format!("{ACTIONS_MOUNT_ROOT}/{owner}/{repo}/{}", sha.as_str())
}

fn register_bind(collector: &mut Collector, host_dir: &Path, container_path: &str) {
    let host_str = host_dir.to_string_lossy().into_owned();
    if collector.seen_dirs.insert(host_str.clone()) {
        collector.binds.push(BindMount {
            host_path: host_str,
            container_path: container_path.to_string(),
            read_only: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greenlit_actions::CommitSha;
    use greenlit_actions::resolve::ResolveError;
    use greenlit_actions::store::{ActionFetcher, ActionStore, FetchError};
    use greenlit_engine::StepPlan;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A [`RefResolver`] that always resolves to the same fixed SHA,
    /// counting calls.
    struct CountingResolver {
        calls: AtomicUsize,
        sha: CommitSha,
    }

    #[async_trait::async_trait]
    impl greenlit_actions::resolve::RefResolver for CountingResolver {
        async fn resolve(
            &self,
            _owner: &str,
            _repo: &str,
            _git_ref: &str,
        ) -> Result<CommitSha, ResolveError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.sha.clone())
        }
    }

    /// An [`ActionFetcher`] that writes a minimal valid Node action manifest,
    /// counting calls — used to prove the store's own presence check (not
    /// this fetcher) answers a second resolution of the same pinned SHA
    /// (`TESTING.md`/`PHASE-3-actions.md` exit criterion 4: "a second run
    /// performs zero network fetches for pinned SHAs").
    struct CountingFetcher {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ActionFetcher for CountingFetcher {
        async fn fetch(
            &self,
            _owner: &str,
            _repo: &str,
            _sha: &CommitSha,
            dest: &Path,
        ) -> Result<(), FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(
                dest.join("action.yml"),
                b"name: fixture\nruns:\n  using: node20\n  main: index.js\n",
            )
            .unwrap();
            Ok(())
        }
    }

    fn uses_step(reference: &str) -> StepPlan {
        StepPlan {
            id: None,
            name: None,
            env: IndexMap::new(),
            working_directory: None,
            continue_on_error: None,
            timeout_minutes: None,
            condition: None,
            implicit_status_gate: true,
            kind: StepKind::Uses {
                reference: reference.to_string(),
                span: test_span(),
                with: IndexMap::new(),
            },
        }
    }

    fn test_span() -> Span {
        greenlit_workflow::Span::new(
            std::sync::Arc::from("ci.yml"),
            greenlit_workflow::Location::new(1, 1),
            greenlit_workflow::Location::new(1, 2),
        )
    }

    fn test_config(
        resolver: Arc<CountingResolver>,
        fetcher: Arc<CountingFetcher>,
        store_root: &Path,
    ) -> ActionRuntimeConfig {
        ActionRuntimeConfig {
            resolver,
            store: ActionStore::at(store_root.to_path_buf()),
            fetcher,
            node_runtime_fetcher: Arc::new(NeverDownloads),
            node_runtime_specs: Arc::new(
                crate::executor::actions::node_runtime::PinnedNodeBundleSpecs,
            ),
            node_runtime_store: super::super::node_runtime::RuntimeStore::at(
                store_root.join("node-runtimes"),
            ),
            github_token: None,
        }
    }

    struct NeverDownloads;
    #[async_trait::async_trait]
    impl super::super::node_runtime::RuntimeBundleFetcher for NeverDownloads {
        async fn download(
            &self,
            spec: &super::super::node_runtime::RuntimeBundleSpec,
        ) -> Result<Vec<u8>, super::super::node_runtime::RuntimeBundleError> {
            Err(super::super::node_runtime::RuntimeBundleError::Download {
                url: spec.url.to_string(),
                message: "not used in this test".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn a_second_resolution_of_the_same_pinned_ref_performs_zero_network_calls() {
        let store_root = tempfile::tempdir().unwrap();
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
            sha: CommitSha::parse(&"a".repeat(40)).unwrap(),
        });
        let fetcher = Arc::new(CountingFetcher {
            calls: AtomicUsize::new(0),
        });
        let config = test_config(
            Arc::clone(&resolver),
            Arc::clone(&fetcher),
            store_root.path(),
        );
        // Pinned by full 40-hex SHA, exactly the "pinned SHAs" case the
        // invariant names: `greenlit_actions::resolve::resolve_ref` already
        // short-circuits a full-SHA ref with zero resolver calls (see that
        // crate's own oracle test), so the resolver call count must stay
        // zero across both resolutions here too — only the fetcher's
        // "already fetched" store check is under test.
        let steps = vec![uses_step(&format!("octo/hello-action@{}", "a".repeat(40)))];
        let repo_host_path = tempfile::tempdir().unwrap();

        let first = resolve_job_actions(&steps, &config, repo_host_path.path(), "/ws")
            .await
            .expect("first resolution");
        assert!(matches!(first.per_step[0], Some(ResolvedUses::Node(_))));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);

        // A second resolution of the identically-pinned SHA (a fresh job
        // instance referencing the same commit — e.g. a second job in the
        // same run, or a re-run) must not touch the fetcher again:
        // `ActionStore::ensure_fetched`'s own presence check short-circuits
        // it (`PHASE-3-actions.md` exit criterion 4).
        let second = resolve_job_actions(&steps, &config, repo_host_path.path(), "/ws")
            .await
            .expect("second resolution");
        assert!(matches!(second.per_step[0], Some(ResolvedUses::Node(_))));
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            0,
            "a full-SHA ref never calls the resolver"
        );
        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            1,
            "the action fetcher must not be called again for an already-fetched pinned SHA"
        );
    }

    #[tokio::test]
    async fn a_docker_uses_reference_marks_the_job_as_needing_a_sibling() {
        let store_root = tempfile::tempdir().unwrap();
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
            sha: CommitSha::parse(&"b".repeat(40)).unwrap(),
        });
        let fetcher = Arc::new(CountingFetcher {
            calls: AtomicUsize::new(0),
        });
        let config = test_config(resolver, fetcher, store_root.path());
        let steps = vec![uses_step("docker://alpine:3.19")];
        let repo_host_path = tempfile::tempdir().unwrap();

        let plan = resolve_job_actions(&steps, &config, repo_host_path.path(), "/ws")
            .await
            .expect("docker:// needs no fetch at all");
        assert!(plan.needs_docker_sibling);
        assert!(matches!(plan.per_step[0], Some(ResolvedUses::Docker(_))));
        assert!(plan.container_images.contains("alpine:3.19"));
    }

    #[tokio::test]
    async fn actions_checkout_locks_the_ref_without_fetching_action_source() {
        let store_root = tempfile::tempdir().unwrap();
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
            sha: CommitSha::parse(&"c".repeat(40)).unwrap(),
        });
        let fetcher = Arc::new(CountingFetcher {
            calls: AtomicUsize::new(0),
        });
        let config = test_config(
            Arc::clone(&resolver),
            Arc::clone(&fetcher),
            store_root.path(),
        );
        let steps = vec![uses_step("actions/checkout@v4")];
        let repo_host_path = tempfile::tempdir().unwrap();

        let plan = resolve_job_actions(&steps, &config, repo_host_path.path(), "/ws")
            .await
            .expect("actions/checkout ref resolves without fetching its source");
        assert!(matches!(plan.per_step[0], Some(ResolvedUses::Checkout)));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            plan.identities.get("actions/checkout@v4"),
            Some(&"c".repeat(40))
        );
    }

    #[tokio::test]
    async fn exceeding_the_composite_depth_cap_fails_with_a_clear_message() {
        // A composite action that references itself would recurse forever;
        // the depth cap catches it (and any other over-deep nesting) with a
        // named, actionable error instead of overflowing the stack.
        let store_root = tempfile::tempdir().unwrap();

        struct SelfNestingFetcher;
        #[async_trait::async_trait]
        impl ActionFetcher for SelfNestingFetcher {
            async fn fetch(
                &self,
                _owner: &str,
                _repo: &str,
                _sha: &CommitSha,
                dest: &Path,
            ) -> Result<(), FetchError> {
                std::fs::write(
                    dest.join("action.yml"),
                    b"name: fixture\nruns:\n  using: composite\n  steps:\n    - uses: octo/self@v1\n",
                )
                .unwrap();
                Ok(())
            }
        }

        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
            sha: CommitSha::parse(&"d".repeat(40)).unwrap(),
        });
        let config = ActionRuntimeConfig {
            resolver,
            store: ActionStore::at(store_root.path().to_path_buf()),
            fetcher: Arc::new(SelfNestingFetcher),
            node_runtime_fetcher: Arc::new(NeverDownloads),
            node_runtime_specs: Arc::new(
                crate::executor::actions::node_runtime::PinnedNodeBundleSpecs,
            ),
            node_runtime_store: super::super::node_runtime::RuntimeStore::at(
                store_root.path().join("node-runtimes"),
            ),
            github_token: None,
        };
        let steps = vec![uses_step("octo/self@v1")];
        let repo_host_path = tempfile::tempdir().unwrap();

        let error = resolve_job_actions(&steps, &config, repo_host_path.path(), "/ws")
            .await
            .expect_err("infinitely self-nesting composite must fail, not recurse forever");
        assert!(error.to_string().contains("nesting limit"), "{error}");
    }
}
