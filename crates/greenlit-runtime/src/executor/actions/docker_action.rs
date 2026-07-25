//! Docker action execution: build/pull through the engine trait, run as a
//! *sibling* container — never through a host Docker socket inside the job
//! container.
//!
//! # Design: sharing the job workspace with a sibling container
//!
//! The job workspace lives *inside* the job container's own mount
//! namespace: Phase 2's overlay isolation merges the read-only repo bind
//! with a container-local writable upper layer (or, under the copy-in
//! fallback, a plain directory `greenlit-init` populates once at boot) —
//! either way, nothing outside that one container's namespace can see the
//! merged result through an ordinary bind mount, because it is not backed
//! by any single host directory a second container could also bind.
//!
//! A Docker action's sibling container is a *second*, independently
//! created container talking to the same daemon (never a socket handed
//! into the job container — `AGENTS.md`), so it needs its own, real bind
//! source for whatever the job's workspace currently looks like.
//!
//! **The chosen design**: when [`super::resolve::JobActionPlan::needs_docker_sibling`]
//! is true, the job's workspace is backed by a run-scoped Docker **named
//! volume** instead of the container-local overlay/copy-in target —
//! [`crate::executor::job::boot_container`] forces that job's isolation
//! strategy to copy-in (unchanged, zero `greenlit-init` code) and binds the
//! volume at the workspace path instead of leaving it as container-local
//! storage; `greenlit-init`'s existing copy-in populate step fills whatever
//! is bind-mounted at the workspace path exactly as it always has, with no
//! awareness that the destination is now a named volume rather than
//! container-local storage. Every Docker action sibling for that job then
//! mounts the *same* volume, read-write, at the *same* workspace path, so a
//! `run:` step's write and a Docker action's write are both visible to
//! every later step regardless of which container performed it.
//!
//! **Tradeoff, stated plainly**: this trades away the overlay's zero-copy
//! start for jobs that use a Docker action specifically (copy-in walks and
//! copies the whole checkout instead of an instant kernel mount) in
//! exchange for a real, host-safe (never a host bind, never a workflow-
//! named target — `crate::executor::container`'s named-volume
//! namespacing applies here too), writable, sibling-shareable workspace.
//! Jobs with no Docker action are entirely unaffected (they keep the
//! default overlay-preferring strategy). The volume is created and removed
//! per job (removal follows the job container's own teardown in
//! `crate::executor::job`, since a volume still bound by a running container
//! cannot be removed), namespaced to the run exactly like a workflow-authored
//! `volumes:` entry, so it can never resolve to a pre-existing daemon-global
//! volume.
//!
//! # Inputs
//!
//! Docker actions receive `with:` the same way JS actions do — one
//! `INPUT_<NAME>` env var per effective input
//! (`super::nodejs::input_env`) — **not** a `${{ inputs.* }}` context,
//! which only composite actions have
//! (<https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#inputs-context>:
//! "This context is only available inside composite actions."). `args:`/
//! `env:`/`entrypoint:` in the action's own manifest are themselves
//! `${{ }}`-templated against the *job's* context (no `inputs` context
//! there either), evaluated at execution time (`super::template`).

use std::io::Write;
use std::path::Path;

use greenlit_engine::execution::Masker;
use greenlit_engine::execution::outcome::StepExit;
use greenlit_expr::Context;
use indexmap::IndexMap;

use crate::engine::{BindMount, ContainerEngine, ContainerSpec};
use crate::executor::ExecError;
use crate::executor::container::namespaced_volume_name;
use crate::executor::logsink::StepLogSink;

use super::nodejs;
use super::resolve::{DockerImageSource, ResolvedDocker};
use super::template;

/// Ensures a Docker action's image exists locally (building from the
/// action's Dockerfile, or pulling a published reference), returning the
/// tag/reference to run.
///
/// # Errors
/// Returns an [`ExecError`] if assembling the build context, the build
/// itself, or the pull fails.
async fn ensure_image(
    engine: &dyn ContainerEngine,
    source: &DockerImageSource,
    ctx: &Context,
) -> Result<String, ExecError> {
    match source {
        DockerImageSource::Pull { image } => {
            let resolved =
                template::resolve_template(image, ctx).map_err(ExecError::template_eval)?;
            if !engine.image_exists(&resolved).await? {
                let mut null = crate::progress::ProgressNull;
                engine.pull_image(&resolved, None, &mut null).await?;
            }
            Ok(resolved)
        }
        DockerImageSource::Build {
            host_action_dir,
            dockerfile,
        } => {
            let tag = format!(
                "greenlit/action-build:{:016x}",
                content_hash(host_action_dir, dockerfile)
            );
            if !engine.image_exists(&tag).await? {
                let context_tar = build_context_tar(host_action_dir).map_err(|source| {
                    ExecError::Infrastructure {
                        message: format!(
                            "could not assemble the Docker action's build context: {source}"
                        ),
                        fix: "ensure the action's source directory is readable".to_string(),
                    }
                })?;
                let spec = crate::engine::BuildSpec {
                    context_tar,
                    dockerfile: dockerfile.clone(),
                    tag: tag.clone(),
                    build_args: Vec::new(),
                };
                let mut null = crate::progress::ProgressNull;
                engine.build_image(&spec, &mut null).await?;
            }
            Ok(tag)
        }
    }
}

/// Everything one Docker action run needs, bundled so [`execute`] takes one
/// parameter instead of an unwieldy positional list.
pub(crate) struct DockerActionRequest<'a> {
    pub engine: &'a dyn ContainerEngine,
    pub resolved: &'a ResolvedDocker,
    pub reference: &'a str,
    /// This step's already-resolved `with:`.
    pub with: &'a IndexMap<String, String>,
    /// The step's already fully-layered environment (base + workflow + job +
    /// `GITHUB_ENV` accumulation + this step's own `env:`).
    pub full_env: &'a IndexMap<String, String>,
    pub ctx: &'a Context,
    pub workspace: &'a str,
    /// The run-scoped named volume backing this job's shared workspace (see
    /// module docs) — the caller only invokes [`execute`] for a job where
    /// [`super::resolve::JobActionPlan::needs_docker_sibling`] was true, so
    /// a volume always exists by the time this runs.
    pub workspace_volume: &'a str,
    pub volume_namespace: &'a str,
}

/// Runs a resolved Docker action's sibling container to completion.
///
/// # Errors
/// Returns an [`ExecError`] on any engine failure (image ensure, sibling
/// create/run/remove) or expression-evaluation failure.
pub(crate) async fn execute(
    request: DockerActionRequest<'_>,
    out: &mut (dyn Write + Send),
    masker: &mut Masker,
) -> Result<StepExit, ExecError> {
    let DockerActionRequest {
        engine,
        resolved,
        reference,
        with,
        full_env,
        ctx,
        workspace,
        workspace_volume,
        volume_namespace,
    } = request;
    let image = ensure_image(engine, &resolved.source, ctx).await?;

    let mut args = Vec::with_capacity(resolved.args.len());
    for arg in &resolved.args {
        args.push(template::resolve_template(arg, ctx).map_err(ExecError::template_eval)?);
    }
    let entrypoint = match &resolved.entrypoint {
        Some(raw) => vec![template::resolve_template(raw, ctx).map_err(ExecError::template_eval)?],
        None => Vec::new(),
    };

    let input_env = nodejs::input_env(reference, &resolved.inputs, with, ctx)?;
    let mut env: IndexMap<String, String> = full_env.clone();
    for (key, value) in input_env {
        env.insert(key, value);
    }
    for (key, raw_value) in &resolved.env {
        let value = template::resolve_template(raw_value, ctx).map_err(ExecError::template_eval)?;
        env.insert(key.clone(), value);
    }

    let spec = ContainerSpec {
        image,
        entrypoint,
        cmd: args,
        env: env.into_iter().collect(),
        working_dir: Some(workspace.to_string()),
        network: None,
        labels: vec![
            ("greenlit.managed".to_string(), "1".to_string()),
            ("greenlit.docker-action".to_string(), "1".to_string()),
        ],
        binds: vec![BindMount {
            host_path: namespaced_volume_name(volume_namespace, workspace_volume),
            container_path: workspace.to_string(),
            read_only: false,
        }],
        name: None,
        // A Docker action gets no capabilities, no published ports, no health
        // probe, and no network alias: it is a sibling that shares only the
        // job's workspace volume.
        ..ContainerSpec::default()
    };

    let id = engine.create_container(&spec).await?;
    let mut sink = StepLogSink::new(out, masker);
    let run_result = engine.run_container(&id, &mut sink).await;
    sink.finish();
    // Best-effort teardown regardless of the run's own outcome — a leaked
    // sibling container must never be the difference between a diagnosable
    // step failure and a silent resource leak.
    let _ = engine.remove_container(&id).await;
    let output = run_result?;
    Ok(if output.exit_code == 0 {
        StepExit::Success
    } else {
        StepExit::Failed
    })
}

/// A deterministic cache key for a Docker action's build, derived from the
/// action's (stable, content-addressed) source directory path and its
/// Dockerfile name. Not a cryptographic digest (like
/// `crate::image::context::content_hash`, which this mirrors): a rebuild
/// cache key, not a security boundary.
fn content_hash(host_action_dir: &Path, dockerfile: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for bytes in [
        host_action_dir.to_string_lossy().as_bytes(),
        dockerfile.as_bytes(),
    ] {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash
}

/// Assembles an uncompressed tar of `host_action_dir`'s entire contents —
/// the build context a Docker action's own `Dockerfile` build needs, read
/// directly from the host (no container bind needed for a build; see
/// module docs).
fn build_context_tar(host_action_dir: &Path) -> std::io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_dir_all(".", host_action_dir)?;
    builder.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_deterministic_and_input_sensitive() {
        let a = content_hash(Path::new("/x/y"), "Dockerfile");
        let b = content_hash(Path::new("/x/y"), "Dockerfile");
        assert_eq!(a, b);
        assert_ne!(a, content_hash(Path::new("/x/z"), "Dockerfile"));
        assert_ne!(a, content_hash(Path::new("/x/y"), "Dockerfile.alt"));
    }

    #[test]
    fn build_context_tar_includes_every_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), b"FROM scratch").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.js"), b"console.log(1)").unwrap();

        let tar_bytes = build_context_tar(dir.path()).unwrap();
        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("Dockerfile")));
        assert!(names.iter().any(|n| n.ends_with("src/lib.js")));
    }
}
