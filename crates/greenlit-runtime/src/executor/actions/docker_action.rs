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
//! # Design: sharing the job container's network namespace
//!
//! A sibling on Docker's default bridge could not resolve a `services:`
//! container by hostname, and — worse — was not covered by the network
//! policy at all: [`crate::executor::netguard`] installs its rules by
//! joining the *target* container's own network namespace
//! (`--network container:<id>`, its module docs' "the mechanism"), and a
//! plain-bridge sibling is a namespace the netguard sidecar never touched.
//!
//! **The chosen design**: the sibling joins the job container's network
//! namespace the same way netguard does —
//! `network: Some(format!("container:{container}"))` — instead of getting a
//! network, IP, and alias of its own. One mechanism buys two things:
//!
//! * The rules netguard already installed into the job container's namespace
//!   apply to the sibling from its first instruction. No per-sibling sidecar
//!   run, and no race between the sibling starting and a policy landing —
//!   the namespace was guarded before the sibling existed.
//! * Docker's embedded DNS (`127.0.0.11`) is owned by the network namespace,
//!   not by the container, so a sibling sharing the job's namespace resolves
//!   a service's id exactly as the job container does — no separate network
//!   attachment needed for the alias [`crate::executor::services`] gives
//!   each service container (`network_aliases: vec![id.clone()]`, ~line
//!   294-319) to resolve.
//!
//! **Tradeoff, stated plainly**: on GitHub's own runner a `uses: docker://`
//! action gets its own IP on the job's network — its own namespace, attached
//! to the same bridge as the job container, with its own loopback. Here the
//! sibling shares the job container's namespace outright: same IP, same
//! loopback. Every fidelity-relevant behavior an action could actually
//! observe is identical either way — it reaches services by id, reaches the
//! internet, and is blocked from the LAN and the cloud metadata address
//! exactly as the job container is — so the divergence is confined to the
//! one thing nothing in a workflow can tell apart from outside: whether
//! `127.0.0.1` inside the action is a loopback it owns alone or one it
//! shares with the job container.
//!
//! # Design: the command files a sibling can write
//!
//! A step's `GITHUB_ENV`/`GITHUB_OUTPUT`/`GITHUB_PATH`/`GITHUB_STEP_SUMMARY`
//! files normally live under [`crate::executor::job`]'s `CMDFILES_BASE`,
//! which is ordinary container-local storage — invisible to a sibling, and
//! therefore unusable by a Docker action. That is why a Docker action's
//! outputs went nowhere before this: the action ran, wrote to a variable
//! that was never set, and every downstream `steps.<id>.outputs.*` silently
//! read empty.
//!
//! The fix mirrors the workspace decision above: a *second* run-scoped named
//! volume, mounted at the **same** `CMDFILES_BASE` path in the job container
//! and in every sibling, so [`crate::executor::cmdfiles`] is reused
//! unchanged — it still materializes and reads the files through the job
//! container, and the paths it hands the sibling resolve to the same bytes.
//!
//! A second volume rather than a subdirectory of the workspace one: command
//! files under `GITHUB_WORKSPACE` would be visible to `git status`, matched
//! by a `hashFiles('**')` pattern, and swept into an `upload-artifact` or an
//! `actions/cache` archive. The workspace has to stay exactly what the
//! workflow checked out.
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
use crate::executor::cmdfiles::{self, CommandFileEffects, CommandFilePaths};
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
    locked_images: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<String, ExecError> {
    match source {
        DockerImageSource::Pull { image } => {
            let resolved =
                template::resolve_template(image, ctx).map_err(ExecError::template_eval)?;
            let materialized = match locked_images {
                Some(locks) => locks.get(&resolved).cloned().ok_or_else(|| {
                    ExecError::Infrastructure {
                        message: format!(
                            "Docker action image '{resolved}' is absent from the finalized RunLock"
                        ),
                        fix: "use a statically resolvable Docker action image or preserve the run directory and file a Greenlit defect"
                            .to_string(),
                    }
                })?,
                None => resolved.clone(),
            };
            if !engine.image_exists(&materialized).await? {
                if locked_images.is_some() {
                    return Err(ExecError::Infrastructure {
                        message: format!(
                            "locked Docker action image '{materialized}' disappeared before step startup"
                        ),
                        fix: "retry to prepare the exact image again; do not prune Docker images during an active run"
                            .to_string(),
                    });
                }
                let mut null = crate::progress::ProgressNull;
                engine.pull_image(&resolved, None, &mut null).await?;
            }
            Ok(materialized)
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

/// The bare (pre-namespacing) names of the two run-scoped named volumes a
/// job containing a Docker action provisions: the shared workspace, and the
/// per-step command files (see module docs). Both are created and removed
/// with the job by [`crate::executor::job`], which owns the names.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SiblingVolumes {
    /// Backs `GITHUB_WORKSPACE`, so a `run:` step's write and a Docker
    /// action's write are both visible to every later step.
    pub workspace: &'static str,
    /// Backs the command-file base, so a Docker action can set outputs,
    /// export env, extend `PATH`, and write a step summary.
    pub cmdfiles: &'static str,
}

/// Everything one Docker action step needs, bundled so [`run_step`] takes
/// one parameter instead of an unwieldy positional list.
pub(crate) struct DockerActionRequest<'a> {
    pub engine: &'a dyn ContainerEngine,
    /// The *job* container. The action itself runs in a sibling, but this
    /// step's command files are materialized and read back through the job
    /// container exactly as every other step kind's are — they are only
    /// reachable from the sibling because both mount the same volume.
    pub container: &'a str,
    pub resolved: &'a ResolvedDocker,
    pub reference: &'a str,
    /// This step's already-resolved `with:`.
    pub with: &'a IndexMap<String, String>,
    /// The step's already fully-layered environment (base + workflow + job +
    /// `GITHUB_ENV` accumulation + this step's own `env:`).
    pub full_env: &'a IndexMap<String, String>,
    pub ctx: &'a Context,
    pub workspace: &'a str,
    /// Mount point of the shared command-file volume — identical in the job
    /// container and in every sibling, which is what makes the paths in
    /// `cmdfiles` mean the same thing on both sides.
    pub cmdfiles_root: &'a str,
    /// This step's own command files, under `cmdfiles_root`.
    pub cmdfiles: &'a CommandFilePaths,
    /// The run-scoped named volumes backing both (see module docs) — the
    /// caller only invokes [`run_step`] for a job where
    /// [`super::resolve::JobActionPlan::needs_docker_sibling`] was true, so
    /// they always exist by the time this runs.
    pub volumes: SiblingVolumes,
    pub volume_namespace: &'a str,
    pub locked_images: Option<&'a std::collections::BTreeMap<String, String>>,
}

/// What one Docker action step produced: how it exited, and whatever it
/// wrote to its command files.
pub(crate) struct DockerActionOutcome {
    pub exit: StepExit,
    pub effects: CommandFileEffects,
}

/// Runs a resolved Docker action as a sibling container, with the same
/// command-file protocol every other step kind gets.
///
/// # Errors
/// Returns an [`ExecError`] on any engine failure (image ensure, sibling
/// create/run/remove), an expression-evaluation failure, or a malformed
/// command file.
pub(crate) async fn run_step(
    request: DockerActionRequest<'_>,
    out: &mut (dyn Write + Send),
    masker: &mut Masker,
) -> Result<DockerActionOutcome, ExecError> {
    let DockerActionRequest {
        engine,
        container,
        resolved,
        reference,
        with,
        full_env,
        ctx,
        workspace,
        cmdfiles_root,
        cmdfiles: paths,
        volumes,
        volume_namespace,
        locked_images,
    } = request;
    let image = ensure_image(engine, &resolved.source, ctx, locked_images).await?;

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
    // The pinned runner's `ContainerActionHandler.RunAsync` calls only
    // `AddInputsToEnvironment()` (ContainerActionHandler.cs:39) for a `uses:`
    // container action — never `AddPrependPathToEnvironment()`, whose only
    // callers are `NodeScriptActionHandler.cs:40` and `ScriptHandler.cs:288`.
    // Prepend-path reaches a container *step* (`jobs.<id>.container`) through
    // a wholly different mechanism, `ContainerStepHost.PrependPath`
    // (`Handler.cs:205-233`), never used for `uses:` actions. So on the real
    // runner a Docker action never sees the job's PATH at all — only the
    // image's own. `full_env` here still carries the *job container's*
    // seeded PATH (`/greenlit/wrappers`, `/greenlit/shims`, and any
    // `GITHUB_PATH` accumulation), which must not leak into the sibling and
    // shadow the action image's own PATH. Dropping the key, rather than
    // forwarding it, is what makes the image's own `ENV PATH` win exactly as
    // it does on the pinned runner (`actions/runner` v2.336.0, pinned
    // release).
    env.shift_remove("PATH");
    for (key, value) in input_env {
        env.insert(key, value);
    }
    for (key, raw_value) in &resolved.env {
        let value = template::resolve_template(raw_value, ctx).map_err(ExecError::template_eval)?;
        env.insert(key.clone(), value);
    }

    // Materialized through the job container, onto the volume the sibling is
    // about to mount. The empty script argument is deliberate: a Docker
    // action has an entrypoint, not a shell script, so `paths.script` exists
    // (`prepare` writes all of them or none) and is simply never read.
    cmdfiles::prepare(engine, container, paths, "")
        .await
        .map_err(|source| ExecError::CommandFile(masker.apply(&source.to_string())))?;
    cmdfiles::open_to_sibling(engine, container, paths)
        .await
        .map_err(|source| ExecError::CommandFile(masker.apply(&source.to_string())))?;
    env.insert("GITHUB_ENV".to_string(), paths.env.clone());
    env.insert("GITHUB_OUTPUT".to_string(), paths.output.clone());
    env.insert("GITHUB_PATH".to_string(), paths.path.clone());
    env.insert("GITHUB_STEP_SUMMARY".to_string(), paths.summary.clone());

    let spec = ContainerSpec {
        image,
        entrypoint,
        cmd: args,
        env: env.into_iter().collect(),
        working_dir: Some(workspace.to_string()),
        // Joins the *job* container's own network namespace — the same
        // `container:<id>` mode `crate::executor::netguard` uses to bind its
        // sidecar's rules into a namespace (see the module docs' "sharing
        // the job container's network namespace" section for why). Without
        // this the sibling landed on Docker's default bridge: unable to
        // resolve a `services:` container by id, and entirely outside the
        // netguard policy, which only ever touches the namespace it was
        // pointed at.
        network: Some(format!("container:{container}")),
        labels: vec![
            ("greenlit.managed".to_string(), "1".to_string()),
            ("greenlit.run".to_string(), volume_namespace.to_string()),
            ("greenlit.docker-action".to_string(), "1".to_string()),
        ],
        binds: vec![
            BindMount {
                host_path: namespaced_volume_name(volume_namespace, volumes.workspace),
                container_path: workspace.to_string(),
                read_only: false,
            },
            BindMount {
                host_path: namespaced_volume_name(volume_namespace, volumes.cmdfiles),
                container_path: cmdfiles_root.to_string(),
                read_only: false,
            },
        ],
        name: None,
        // A Docker action gets no capabilities, no published ports, no
        // health probe, and no network alias of its own — `network` above
        // already puts it in the job container's namespace, so it is a
        // sibling that shares the job's workspace and command-file volumes
        // *and* its network.
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
    // Collected even when the action failed: GitHub reads a failed step's
    // command files too, and an action that sets an output and *then* exits
    // non-zero (with `continue-on-error`, say) still published that output.
    let effects = cmdfiles::collect(engine, container, paths)
        .await
        .map_err(|source| ExecError::CommandFile(masker.apply(&source.to_string())))?;
    Ok(DockerActionOutcome {
        exit: if output.exit_code == 0 {
            StepExit::Success
        } else {
            StepExit::Failed
        },
        effects,
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
