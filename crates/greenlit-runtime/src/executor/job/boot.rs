//! Getting the job container running: settling on the image it starts from
//! (an immutable Greenlit runner profile or a validated user-declared
//! `container:`), creating and starting the container with its
//! binds/env/network, and seeding its live `PATH` before the step loop
//! begins.

use indexmap::IndexMap;
use tracing::Instrument;

use greenlit_engine::execution::Masker;
use greenlit_engine::execution::NeedRecord;
use greenlit_expr::{RunStatus, Value};

use crate::engine::{BindMount, ContainerEngine, ExecSpec};
use crate::executor::container::{ContainerAdditions, namespaced_volume_name, validate_container};
use crate::executor::context::CaptureSink;
use crate::executor::dind;
use crate::executor::instance::JobInstance;
use crate::executor::readiness::READY_MARKER;
use crate::executor::runner_profile;
use crate::executor::services;
use crate::executor::{ExecError, Shared, stage_span};
use crate::image::{INIT_IN_IMAGE_PATH, init_binary};
use crate::isolation::{IsolationStrategy, isolation_container_spec};
use crate::progress::{ProgressEvent, ProgressSink};

/// What `resolve_image` settled on for this job.
pub(super) struct ResolvedImage {
    /// The exact immutable image the container starts from.
    pub(super) tag: String,
    /// Whether this is a user-declared `container:`.
    pub(super) in_container: bool,
    /// Whether `bash` is known to be present.
    pub(super) bash_available: bool,
    /// Validated job-container additions.
    pub(super) additions: ContainerAdditions,
}

/// Ensure the job's image exists and, for a job container, validate it.
///
/// Returns `(image_tag, in_container, bash_available, additions)`.
pub(super) async fn resolve_image(
    shared: &Shared<'_>,
    masker: &mut Masker,
    runner_ctx: &Value,
    instance: &JobInstance<'_>,
    base_env: &IndexMap<String, String>,
    needs: &[NeedRecord],
    progress: &mut (dyn ProgressSink + Send),
) -> Result<ResolvedImage, ExecError> {
    match instance.container {
        Some(container_plan) => {
            let ctx = super::env_ctx(
                shared.roots,
                runner_ctx,
                &instance.matrix,
                base_env,
                needs,
                RunStatus::Success,
            );
            let resolved = super::resolve_container(container_plan, &ctx)?;
            // A resolved registry password/username may not already be a
            // registered secret (e.g. a literal in `credentials:`, however
            // inadvisable) — mask both before anything derived from them
            // (the pull's progress events, a later error) can reach output,
            // matching "never log them" (`PHASE-3-actions.md`).
            if let Some(credentials) = &resolved.credentials {
                masker.add(&credentials.username);
                masker.add(&credentials.password);
            }
            let additions = validate_container(
                &resolved,
                &shared.config.workspace,
                &shared.config.volume_namespace,
            )?;
            let locked_image = shared.config.locked_image(&resolved.image)?;
            // Pull only when absent, so a present image (and an offline host)
            // still runs, and re-runs skip the registry round-trip. The image
            // reference is expression-resolved, so it is masked before it can
            // reach a progress display.
            let masked_image = masker.apply(&resolved.image);
            let ensure = async {
                if !shared.engine.image_exists(&locked_image).await? {
                    if shared.config.locked_images.is_some() {
                        return Err(ExecError::Infrastructure {
                            message: format!(
                                "locked container image '{locked_image}' disappeared before job startup"
                            ),
                            fix: "retry to prepare the exact image again; do not prune Docker images during an active run"
                                .to_string(),
                        });
                    }
                    let mut masked = MaskedPullSink {
                        inner: progress,
                        masked_image,
                    };
                    shared
                        .engine
                        .pull_image(&locked_image, additions.registry_auth.as_ref(), &mut masked)
                        .await?;
                }
                Ok::<_, ExecError>(())
            };
            ensure.instrument(stage_span("image-ensure")).await?;
            // A job container image is not guaranteed to ship bash; GitHub
            // defaults such jobs to `sh`.
            Ok(ResolvedImage {
                tag: locked_image,
                in_container: true,
                bash_available: false,
                additions,
            })
        }
        None => {
            let tag = runner_profile::for_runner(instance.runner)
                .image
                .to_string();
            if !shared.engine.image_exists(&tag).await? {
                return Err(ExecError::Infrastructure {
                    message: format!(
                        "locked runner profile '{tag}' disappeared before job startup"
                    ),
                    fix: "retry to prepare the exact runner profile again; do not prune Docker images during an active run"
                        .to_string(),
                });
            }
            Ok(ResolvedImage {
                tag,
                in_container: false,
                bash_available: true,
                additions: ContainerAdditions::default(),
            })
        }
    }
}

/// Replaces the image reference in pull events with its masked form before
/// forwarding — the reference can interpolate `::add-mask::`ed values.
struct MaskedPullSink<'a> {
    inner: &'a mut (dyn ProgressSink + Send),
    masked_image: String,
}

impl ProgressSink for MaskedPullSink<'_> {
    fn on_progress(&mut self, event: ProgressEvent) {
        let event = match event {
            ProgressEvent::PullStarted { .. } => ProgressEvent::PullStarted {
                image: self.masked_image.clone(),
            },
            ProgressEvent::PullFinished {
                downloaded_bytes, ..
            } => ProgressEvent::PullFinished {
                image: self.masked_image.clone(),
                downloaded_bytes,
            },
            other => other,
        };
        self.inner.on_progress(event);
    }
}

/// Queries the freshly booted (and now ready) job container's own default
/// `PATH` — inherited from the image, before any step or `GITHUB_PATH`
/// mutation — and seeds it into `base_env`.
///
/// Every later step's environment is built by
/// [`greenlit_engine::execution::env::layer_step_env`] plus
/// [`apply_path_additions`], and that function's documented contract is to
/// *prepend* `GITHUB_PATH` additions onto whatever `PATH` the layered map
/// already carries — never to invent one. Before this seed, no layer ever
/// carried an explicit `PATH` key at all (`base_env` is built from
/// [`RunnerEnv::into_map`], which has no `PATH` field), so the container's
/// real `PATH` was reaching every step only through ordinary Docker `exec`
/// environment inheritance, never through Greenlit's own layering. That
/// works for the *first* step of a job, but the instant one step calls
/// `core.addPath()` (`setup-node` and most `setup-*` actions do), the caller
/// starts passing an explicit `PATH=<additions>` entry into every later
/// `exec`, which *overrides* the container's inherited default outright
/// (Docker merges an exec's explicit env over the container's live
/// environment key-for-key) — silently dropping `/usr/bin` and friends from
/// every subsequent step. GitHub's real runner never has this gap: its
/// `ExecutionContext` tracks one explicit, always-current `PATH` value from
/// the job's own start (seeded from the runner process's own environment),
/// which `core.addPath` only ever prepends onto. Querying the booted
/// container's own `PATH` once, up front, and seeding it into `base_env`
/// gives Greenlit's layering that same explicit, always-current baseline —
/// container-agnostic, so it works identically for a locked runner profile
/// and an arbitrary user-specified `jobs.<id>.container`.
///
/// # Errors
///
/// Returns an [`ExecError`] if the query exec itself could not be dispatched
/// (an infrastructure failure, not a step failure) — a non-zero exit or
/// empty output is treated as "no baseline available" rather than a hard
/// failure, leaving `base_env` without `PATH` (the pre-fix behavior).
pub(crate) async fn seed_container_path(
    engine: &dyn ContainerEngine,
    container: &str,
    base_env: &mut IndexMap<String, String>,
) -> Result<(), ExecError> {
    let spec = ExecSpec {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s' \"$PATH\"".to_string(),
        ],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = CaptureSink::default();
    let output = engine.exec(container, &spec, &mut sink).await?;
    if output.exit_code == 0 {
        let mut path = sink.text();
        // The image is untrusted, and this read was previously unbounded --
        // every HTTP path in the codebase caps its body read for the same
        // reason. A `PATH` longer than this is not a real one.
        if path.len() > super::MAX_PATH_BYTES {
            path.truncate(super::MAX_PATH_BYTES);
            // Truncation can land mid-entry; drop the partial tail rather
            // than leaving a directory name that means something else.
            if let Some(last) = path.rfind(':') {
                path.truncate(last);
            }
        }
        if !path.is_empty() {
            // The managed `docker` wrapper has to beat a CLI the job image
            // already ships (a `docker:27-cli` job container has one at
            // `/usr/local/bin`).
            base_env.insert("PATH".to_string(), format!("{}:{path}", dind::WRAPPER_DIR));
        }
    }
    Ok(())
}

/// Create and start the isolated job container, returning its id.
///
/// `needs_docker_sibling` forces this job's workspace isolation to copy-in
/// (regardless of the run's requested strategy) and binds two run-scoped
/// named volumes — one at the workspace path instead of leaving it
/// container-local, one at [`super::CMDFILES_BASE`] — so a Docker action's
/// sibling container can mount the *same* volumes and share both the
/// checkout and the step's command files
/// (`crate::executor::actions::docker_action` module docs). `greenlit-init`
/// itself needs no change: its copy-in populate step fills whatever is
/// already bind-mounted at the workspace path, oblivious to whether that is
/// container-local storage or a named volume.
/// Everything that shapes the job container, gathered so the boot call keeps
/// one argument per *concern* rather than one per field.
pub(crate) struct BootRequest<'a> {
    /// The resolved image reference.
    pub(super) image: &'a str,
    /// Whether this is a user-authored job container.
    pub(super) in_container: bool,
    /// Job-container `env:`/`volumes:`/credentials, already validated.
    pub(super) additions: &'a ContainerAdditions,
    /// Read-only binds the job's `uses:` steps need.
    pub(super) extra_binds: &'a [BindMount],
    /// Whether a Docker action forces the shared-workspace volume.
    pub(super) needs_docker_sibling: bool,
    /// The job's own bridge network.
    pub(super) network: &'a str,
}

pub(crate) async fn boot_container(
    shared: &Shared<'_>,
    request: &BootRequest<'_>,
    progress: &mut (dyn ProgressSink + Send),
) -> Result<String, ExecError> {
    let BootRequest {
        image,
        in_container,
        additions,
        extra_binds,
        needs_docker_sibling,
        network,
    } = *request;
    let repo = shared.config.repo_host_path.to_string_lossy().into_owned();
    let idle = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("touch {READY_MARKER}; exec tail -f /dev/null"),
    ];
    let strategy = if needs_docker_sibling {
        IsolationStrategy::CopyIn
    } else {
        shared.config.strategy
    };
    let mut spec = isolation_container_spec(
        image.to_string(),
        &repo,
        &shared.config.workspace,
        strategy,
        idle,
    );
    if !in_container {
        spec.user = Some("0:0".to_string());
    }
    // Runner profiles and job-container images are immutable external OCI
    // content. Inject the private helper read-only rather than rebuilding or
    // mutating either image.
    let helper = write_helper_binary()?;
    spec.binds.push(BindMount {
        host_path: helper,
        container_path: INIT_IN_IMAGE_PATH.to_string(),
        read_only: true,
    });
    if needs_docker_sibling {
        for (source, mount_point) in [
            (
                super::DOCKER_SIBLING_VOLUMES.workspace,
                shared.config.workspace.clone(),
            ),
            (
                super::DOCKER_SIBLING_VOLUMES.cmdfiles,
                super::CMDFILES_BASE.to_string(),
            ),
        ] {
            spec.binds.push(BindMount {
                host_path: namespaced_volume_name(&shared.config.volume_namespace, source),
                container_path: mount_point,
                read_only: false,
            });
        }
    }
    spec.binds.extend(additions.volume_binds.iter().cloned());
    spec.binds.extend(extra_binds.iter().cloned());
    spec.env = additions.env.clone();
    spec.network = Some(network.to_string());
    // The persistent toolcache: a writable Greenlit-owned host directory, so a
    // `setup-*` action finds what a previous run installed instead of
    // downloading it again (`PHASE-4-environment.md`: "mount
    // `~/.litci/toolcache` at `RUNNER_TOOL_CACHE`"). A failure to create it
    // means no toolcache this run, not a failed run.
    if let Some(store) = shared.config.store.as_ref() {
        match services::toolcache_bind(
            &store.toolcache_root,
            &shared.config.runner_env.runner_tool_cache,
        ) {
            Ok(bind) => spec.binds.push(bind),
            Err(error) => tracing::debug!(
                target: "greenlit_runtime::services",
                %error,
                "could not prepare the toolcache; running without it"
            ),
        }
        match services::cargo_download_binds(&store.package_cache_root) {
            Ok(binds) => spec.binds.extend(binds),
            Err(error) => tracing::debug!(
                target: "greenlit_runtime::services",
                %error,
                "could not prepare Cargo download caches; running without them"
            ),
        }
    }
    spec.labels = vec![("greenlit.managed".to_string(), "1".to_string())];

    let engine = shared.engine;
    progress.on_progress(ProgressEvent::BootStarted);
    let boot = async {
        let id = engine.create_container(&spec).await?;
        engine.start_container(&id).await?;
        Ok::<_, ExecError>(id)
    };
    let id = boot.instrument(stage_span("container-boot")).await?;
    progress.on_progress(ProgressEvent::BootFinished);
    Ok(id)
}

/// Write the embedded `greenlit-init` bytes to a host temp file (mode 0755) so
/// they can be bind-mounted into a job container.
fn write_helper_binary() -> Result<String, ExecError> {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("greenlit-init-{}-{nanos}", std::process::id()));
    std::fs::write(&path, init_binary()).map_err(helper_io_error)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .map_err(helper_io_error)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Map a helper-staging I/O error onto an [`ExecError`] with a fix.
fn helper_io_error(source: std::io::Error) -> ExecError {
    ExecError::Infrastructure {
        message: format!("could not stage the greenlit-init helper: {source}"),
        fix: "ensure the system temporary directory is writable".to_string(),
    }
}
