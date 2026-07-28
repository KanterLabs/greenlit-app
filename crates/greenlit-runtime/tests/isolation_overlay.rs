//! Real-daemon overlay-isolation invariant: the *overlay* half of
//! `PHASE-2-execution.md` exit criterion 2.
//!
//! `isolation.rs` exercises the copy-in path (the fallback available
//! everywhere) and proves `--strategy overlay` fails loudly when overlay is
//! unavailable. This file covers the complementary behaviour TESTING.md's
//! isolation-path clause demands — the overlay path exercised "where the
//! environment allows it (self-hosted or privileged job)" — so a hostile
//! `rm -rf "$GITHUB_WORKSPACE"` running through a *live* overlay mount leaves the
//! host tree byte-for-byte unchanged.
//!
//! This environment's container rootfs is itself overlayfs, which the kernel
//! refuses to use as an overlay upper layer. The overlay mount therefore needs
//! (a) `CAP_SYS_ADMIN` — a privileged container — and (b) a tmpfs holding the
//! upper/work dirs, off the overlay rootfs. That is exactly the "privileged job"
//! TESTING.md names. The product never runs privileged workflow containers (the
//! security model forbids it, and `ContainerSpec` cannot express it), so this
//! one container is created through bollard directly; the real `greenlit-init`
//! overlay code path then runs inside it, driven through the ordinary engine
//! `exec`. The engine is a true external, so it is used real, not faked
//! (`TESTING.md`).
//!
//! A selected live-runtime job promises the privileged-container and overlayfs
//! capabilities. Refusal of either prerequisite is therefore a hard failure,
//! never a passing unexercised path.

#[path = "dockerkit/engine.rs"]
mod engine_support;
#[path = "dockerkit/repo.rs"]
mod repo_support;
#[path = "dockerkit/sink.rs"]
mod sink_support;

use std::collections::HashMap;
use std::path::Path;

use bollard::Docker;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::CreateContainerOptionsBuilder;

use greenlit_runtime::detect::Endpoint;
use greenlit_runtime::engine::{ContainerEngine, ExecSpec};
use greenlit_runtime::isolation::{CONTAINER_LOWER, CONTAINER_UPPER_BASE};
use greenlit_runtime::{DockerEngine, ProgressNull, UbuntuRelease, ensure_base_image};

use engine_support::{required_engine, unique_suffix};
use repo_support::{seed_repo, tree_fingerprint};
use sink_support::CollectSink;

/// In-container workspace: the merged, writable checkout the job sees.
const WORKSPACE: &str = "/workspace";

const TEST: &str = "overlay_isolation_protects_the_host";

#[tokio::test]
async fn overlay_isolation_protects_the_host() {
    let engine = required_engine(TEST).await;

    let tag = ensure_base_image(&engine, UbuntuRelease::Noble2404, &mut ProgressNull)
        .await
        .expect("base image");

    let repo = seed_repo("ovl");
    let before = tree_fingerprint(&repo);

    // A privileged container with a tmpfs at the overlay upper base, so the
    // kernel overlay mount can actually succeed here.
    let (raw, id) = create_privileged_overlay_container(&tag, &repo)
        .await
        .expect("the live overlay job must allow its required privileged test container");

    let outcome = run_overlay_check(&engine, &id).await;

    // Tear down (through the raw client that owns the container) before
    // asserting, so a panic never leaks a privileged container.
    raw.remove_container(
        &id,
        Some(
            bollard::query_parameters::RemoveContainerOptionsBuilder::new()
                .force(true)
                .build(),
        ),
    )
    .await
    .expect("remove the privileged overlay test container");

    let check = outcome.expect("overlay check ran");

    // The invariant, asserted regardless of whether overlay mounted: the hostile
    // step never reaches the host tree.
    assert_eq!(
        before,
        tree_fingerprint(&repo),
        "host tree must be untouched"
    );
    let _ = std::fs::remove_dir_all(&repo);

    assert!(
        check.overlay_mounted,
        "{TEST}: the live overlay job must provide an overlayfs-capable kernel and tmpfs upper"
    );

    // Overlay genuinely mounted: assert the full host-protection story.
    assert!(
        check.saw_canary,
        "overlay merged the read-only lower layer into the workspace"
    );
    assert!(
        check.workspace_emptied,
        "the hostile rm -rf wiped the overlay workspace view"
    );
    assert!(
        check.lower_intact,
        "the read-only lower layer is untouched after the hostile step"
    );
}

/// Results of the in-container overlay check.
struct OverlayCheck {
    overlay_mounted: bool,
    saw_canary: bool,
    workspace_emptied: bool,
    lower_intact: bool,
}

/// Start the privileged container and run the real `greenlit-init` overlay path
/// with a hostile `rm -rf` as the job command, through the ordinary engine exec.
async fn run_overlay_check(
    engine: &DockerEngine,
    id: &str,
) -> Result<OverlayCheck, greenlit_runtime::RuntimeError> {
    engine.start_container(id).await?;

    // `--strategy overlay` requires the mount: on success `greenlit-init` execs
    // the script (printing the markers); on failure it exits non-zero before the
    // script runs (no markers). `rm -rf` cannot unlink the workspace mountpoint
    // itself ("device busy"), but it empties the merged view, so the canary
    // disappears from the workspace while the read-only lower keeps it.
    let script = format!(
        "test -f {WORKSPACE}/canary.txt && echo HAVE_CANARY; \
         rm -rf {WORKSPACE} 2>/dev/null; \
         test -e {WORKSPACE}/canary.txt || echo CANARY_GONE; \
         test -f {CONTAINER_LOWER}/canary.txt && echo LOWER_INTACT"
    );
    let mut sink = CollectSink::default();
    let spec = ExecSpec {
        cmd: vec![
            "/greenlit/bin/greenlit-init".to_string(),
            "--lower".to_string(),
            CONTAINER_LOWER.to_string(),
            "--upper".to_string(),
            CONTAINER_UPPER_BASE.to_string(),
            "--workspace".to_string(),
            WORKSPACE.to_string(),
            "--strategy".to_string(),
            "overlay".to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script,
        ],
        ..ExecSpec::default()
    };
    engine.exec(id, &spec, &mut sink).await?;
    let out = sink.out();

    Ok(OverlayCheck {
        // The canary is only visible through a live overlay merge of the lower.
        overlay_mounted: out.contains("HAVE_CANARY"),
        saw_canary: out.contains("HAVE_CANARY"),
        workspace_emptied: out.contains("CANARY_GONE"),
        lower_intact: out.contains("LOWER_INTACT"),
    })
}

/// Create a privileged container whose overlay upper base is a tmpfs, with the
/// seeded repo bound read-only as the overlay lower layer. Returns the bollard
/// client that owns it (for teardown) and the container id.
async fn create_privileged_overlay_container(
    image: &str,
    repo: &Path,
) -> Result<(Docker, String), bollard::errors::Error> {
    let docker = Docker::connect_with_unix(
        Endpoint::DOCKER_SOCKET_PATH,
        120,
        bollard::API_DEFAULT_VERSION,
    )?;

    // A tmpfs at the upper base keeps the overlay upper/work dirs off the
    // container's own overlayfs rootfs, which the kernel refuses as an upper.
    let mut tmpfs = HashMap::new();
    tmpfs.insert(CONTAINER_UPPER_BASE.to_string(), String::new());

    let host_config = HostConfig {
        privileged: Some(true),
        tmpfs: Some(tmpfs),
        binds: Some(vec![format!(
            "{}:{CONTAINER_LOWER}:ro",
            repo.to_string_lossy()
        )]),
        ..Default::default()
    };
    // Idle on `sleep` so the overlay exec can run inside the live container.
    let body = ContainerCreateBody {
        image: Some(image.to_string()),
        entrypoint: Some(vec!["sleep".to_string()]),
        cmd: Some(vec!["300".to_string()]),
        host_config: Some(host_config),
        ..Default::default()
    };
    let name = format!("greenlit-ovl-{}", unique_suffix());
    let options = CreateContainerOptionsBuilder::new().name(&name).build();
    let created = docker.create_container(Some(options), body).await?;
    Ok((docker, created.id))
}
