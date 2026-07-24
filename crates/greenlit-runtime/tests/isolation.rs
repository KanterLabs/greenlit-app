//! Real-daemon overlay-isolation invariants, exercised through the base image
//! and the real `greenlit-init` entrypoint.
//!
//! `PHASE-2-execution.md` exit criterion 2: a step running
//! `rm -rf "$GITHUB_WORKSPACE"` succeeds while the host tree hash stays
//! unchanged. This environment's containers cannot mount overlayfs (the
//! container rootfs is itself overlay, so an overlay upper is rejected), so the
//! `auto` strategy falls back to copy-in and logs it, and an explicit
//! `--strategy overlay` fails loudly rather than silently degrading — both
//! required behaviours. The copy-in path — which protects the host everywhere —
//! is asserted directly. `TESTING.md` isolation-path coverage: copy-in is
//! exercised here; the live kernel overlay mount protecting the host is
//! exercised in `isolation_overlay.rs` (which provisions the privileged,
//! tmpfs-upper container the mount needs), and the strategy decision itself is
//! covered by `greenlit-init`'s decision unit tests.

mod dockerkit;

use greenlit_runtime::engine::{BindMount, ContainerEngine, ContainerSpec, ExecSpec};
use greenlit_runtime::isolation::CONTAINER_LOWER;
use greenlit_runtime::progress::ProgressNull;
use greenlit_runtime::{UbuntuRelease, ensure_base_image};

use dockerkit::{
    CollectSink, engine_if_reachable, notice_no_daemon, seed_repo, tree_fingerprint, unique_suffix,
};

/// The marker `greenlit-init` prints to stderr on the copy-in fallback
/// (`greenlit_init::FALLBACK_MARKER`; that crate is not a dependency here, so
/// the stable string is asserted directly).
const FALLBACK_MARKER: &str = "GREENLIT_INIT_FALLBACK";

/// In-container workspace (the merged, writable checkout the job sees).
const WORKSPACE: &str = "/workspace";

#[tokio::test]
async fn overlay_isolation_protects_the_host_across_strategies() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("overlay_isolation_protects_the_host_across_strategies");
        return;
    };

    let tag = ensure_base_image(&engine, UbuntuRelease::Noble2404, &mut ProgressNull)
        .await
        .expect("base image");

    let repo = seed_repo("iso");
    let before = tree_fingerprint(&repo);

    // A container idling on the read-only repo bind; each check execs the real
    // helper against it. Overriding the entrypoint to `sleep` keeps the
    // container alive so several strategies can be exercised in one container.
    let name = format!("greenlit-iso-{}", unique_suffix());
    let spec = ContainerSpec {
        image: tag,
        name: Some(name),
        entrypoint: vec!["sleep".to_string()],
        cmd: vec!["300".to_string()],
        binds: vec![BindMount {
            host_path: repo.to_string_lossy().into_owned(),
            container_path: CONTAINER_LOWER.to_string(),
            read_only: true,
        }],
        ..ContainerSpec::default()
    };
    let id = engine.create_container(&spec).await.expect("create");

    let outcome = run_all_checks(&engine, &id).await;

    // Tear down before asserting so a failure never leaks a container.
    let _ = engine.remove_container(&id).await;
    let checks = outcome.expect("checks ran");

    // The host repo is byte-for-byte unchanged after every hostile step.
    assert_eq!(
        before,
        tree_fingerprint(&repo),
        "host tree must be untouched"
    );
    let _ = std::fs::remove_dir_all(&repo);

    checks.assert_all();
}

/// Results of the four in-container checks.
struct Checks {
    copyin_saw_canary: bool,
    copyin_removed: bool,
    copyin_exit: i64,
    auto_logged_fallback: bool,
    auto_removed: bool,
    overlay_required_failed: bool,
    overlay_required_did_not_run: bool,
    ro_bind_rejected_write: bool,
}

impl Checks {
    fn assert_all(&self) {
        // copy-in: the checkout was materialised, the hostile `rm -rf` ran green.
        assert!(self.copyin_saw_canary, "copy-in populated the workspace");
        assert!(self.copyin_removed, "the rm -rf step ran");
        assert_eq!(
            self.copyin_exit, 0,
            "rm -rf workspace succeeds under copy-in"
        );

        // auto: overlay unavailable here → logged copy-in fallback, still green.
        assert!(
            self.auto_logged_fallback,
            "auto strategy logged the copy-in fallback"
        );
        assert!(self.auto_removed, "auto fallback still ran the step");

        // overlay required: fails loudly, and the job command never runs.
        assert!(
            self.overlay_required_failed,
            "--strategy overlay fails when overlay is unavailable"
        );
        assert!(
            self.overlay_required_did_not_run,
            "a required-but-failed overlay never runs the job command"
        );

        // defense in depth: the repo bind is read-only at the Docker level.
        assert!(
            self.ro_bind_rejected_write,
            "the repo lower bind rejects writes"
        );
    }
}

/// Start the container and run every isolation check as a helper exec.
async fn run_all_checks(
    engine: &dyn ContainerEngine,
    id: &str,
) -> Result<Checks, greenlit_runtime::RuntimeError> {
    engine.start_container(id).await?;

    // copy-in: prove the workspace is populated, then delete it.
    let copyin = init_exec(
        engine,
        id,
        "copy-in",
        &format!("test -f {WORKSPACE}/canary.txt && echo HAVE; rm -rf {WORKSPACE}; echo REMOVED"),
    )
    .await?;

    // auto: overlay fails here → fallback marker on stderr, step still runs.
    let auto = init_exec(
        engine,
        id,
        "auto",
        &format!("rm -rf {WORKSPACE}; echo REMOVED"),
    )
    .await?;

    // overlay required: must fail before the command runs.
    let overlay = init_exec(engine, id, "overlay", "echo SHOULD_NOT_RUN").await?;

    // read-only bind: a write into the lower layer is refused.
    let mut ro = CollectSink::default();
    let ro_spec = ExecSpec {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("touch {CONTAINER_LOWER}/should_not_exist 2>&1; echo rc=$?"),
        ],
        ..ExecSpec::default()
    };
    engine.exec(id, &ro_spec, &mut ro).await?;
    let ro_out = ro.out();

    Ok(Checks {
        copyin_saw_canary: copyin.0.contains("HAVE"),
        copyin_removed: copyin.0.contains("REMOVED"),
        copyin_exit: copyin.2,
        auto_logged_fallback: auto.1.contains(FALLBACK_MARKER),
        auto_removed: auto.0.contains("REMOVED"),
        overlay_required_failed: overlay.2 != 0,
        overlay_required_did_not_run: !overlay.0.contains("SHOULD_NOT_RUN"),
        ro_bind_rejected_write: ro_out.contains("Read-only file system")
            || !ro_out.contains("rc=0"),
    })
}

/// Exec the real `greenlit-init` with a strategy and a shell command, returning
/// `(stdout, stderr, exit_code)`.
async fn init_exec(
    engine: &dyn ContainerEngine,
    id: &str,
    strategy: &str,
    shell: &str,
) -> Result<(String, String, i64), greenlit_runtime::RuntimeError> {
    let mut sink = CollectSink::default();
    let spec = ExecSpec {
        cmd: vec![
            "/greenlit/bin/greenlit-init".to_string(),
            "--lower".to_string(),
            CONTAINER_LOWER.to_string(),
            "--upper".to_string(),
            "/greenlit/upper".to_string(),
            "--workspace".to_string(),
            WORKSPACE.to_string(),
            "--strategy".to_string(),
            strategy.to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            shell.to_string(),
        ],
        ..ExecSpec::default()
    };
    let out = engine.exec(id, &spec, &mut sink).await?;
    Ok((sink.out(), sink.err(), out.exit_code))
}
