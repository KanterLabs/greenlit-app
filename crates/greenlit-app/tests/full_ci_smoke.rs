//! `fixtures/full-ci` run twice, end to end, against a real daemon.
//!
//! `docs/PHASE-4-environment.md` exit criterion 1: "workflow with a postgres
//! service, `actions/cache` (miss → save → hit across two runs), artifact
//! upload + download across jobs, and a dynamically selected tool absent from
//! the slim base — `litci run` green twice; second run shows cache hit and
//! converged-image reuse."
//!
//! # Why twice, and why one sandbox
//!
//! Every claim here is about state *surviving between runs*. A cache that
//! saves is worth nothing if the next run does not restore it; a converged
//! image is worth nothing if the next run reinstalls anyway. `Sandbox` gives
//! one isolated `$HOME`, reused across both invocations, which is the only
//! reason `~/.litci/cache` and the per-repo image are visible to the second
//! run at all.
//!
//! # Why this is opt-in
//!
//! It pulls `postgres:16-alpine`, fetches three marketplace actions, runs a
//! privileged-free container stack, and takes about a minute. Gated behind
//! `LITCI_TEST_LIVE_FULL_CI=1` plus a live daemon, each with a notice and an
//! early return — never `#[ignore]`, which `AGENTS.md` bans:
//!
//! ```text
//! LITCI_TEST_LIVE_FULL_CI=1 cargo test -p greenlit-app --test full_ci_smoke
//! ```

pub mod support;

use std::path::{Path, PathBuf};

use support::Sandbox;

const LIVE_ENV_VAR: &str = "LITCI_TEST_LIVE_FULL_CI";

fn fixture_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    std::fs::canonicalize(format!("{manifest}/../../fixtures/full-ci"))
        .expect("fixtures/full-ci exists")
}

/// Recursively copies the committed fixture tree into the sandbox, preserving
/// permission bits the way `actions_ci_smoke` does.
fn copy_fixture_into(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create destination directory");
    for entry in std::fs::read_dir(src).expect("read fixture directory") {
        let entry = entry.expect("fixture directory entry");
        let file_type = entry.file_type().expect("entry file type");
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_fixture_into(&entry.path(), &dst_path);
        } else {
            std::fs::copy(entry.path(), &dst_path).expect("copy fixture file");
        }
    }
}

/// Whether a Docker daemon answers right now.
fn docker_daemon_reachable() -> bool {
    use greenlit_runtime::DockerEngine;
    use greenlit_runtime::detect::Endpoint;
    use greenlit_runtime::engine::ContainerEngine;

    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    runtime.block_on(async {
        let Ok(engine) = DockerEngine::connect(&Endpoint::DockerSocket) else {
            return false;
        };
        engine
            .image_exists("greenlit/probe:definitely-absent")
            .await
            .is_ok()
    })
}

#[test]
fn full_ci_fixture_is_green_twice_and_reuses_what_it_built() {
    if std::env::var_os(LIVE_ENV_VAR).is_none() {
        eprintln!(
            "full_ci_fixture_is_green_twice_and_reuses_what_it_built: skipped \
             (set {LIVE_ENV_VAR}=1 to run the real-daemon full-ci smoke test)"
        );
        return;
    }
    if !docker_daemon_reachable() {
        eprintln!(
            "full_ci_fixture_is_green_twice_and_reuses_what_it_built: \
             no Docker daemon reachable; skipping"
        );
        return;
    }

    let sandbox = Sandbox::new();
    copy_fixture_into(&fixture_root(), sandbox.root());
    sandbox.init_git();

    // ---- Run 1: everything cold ----
    let first = sandbox.run(&["run", "--no-input"]);
    let first_out = support::stdout_text(&first);
    let first_err = support::stderr_text(&first);
    assert!(
        first.status.success(),
        "first run failed\nstdout:\n{first_out}\nstderr:\n{first_err}"
    );

    // The action's own words, not the fixture's echo: an earlier round of
    // debugging read the fixture's `cache miss` line and inferred a 204 that
    // had not happened, when the truth was a hit whose download was refused.
    assert!(
        first_out.contains("Cache not found for input keys"),
        "run 1 must be a genuine cache miss: {first_out}"
    );
    assert!(
        first_out.contains("greenlit: installed"),
        "run 1 provisions the tool the slim base lacks: {first_out}"
    );
    assert!(
        first_out.contains("all checks passed"),
        "the dependent job's artifact check must run: {first_out}"
    );

    // ---- Run 2: same $HOME, so the cache and the image are there ----
    let second = sandbox.run(&["run", "--no-input"]);
    let second_out = support::stdout_text(&second);
    let second_err = support::stderr_text(&second);
    assert!(
        second.status.success(),
        "second run failed\nstdout:\n{second_out}\nstderr:\n{second_err}"
    );

    assert!(
        second_out.contains("Cache restored from key"),
        "run 2 must restore what run 1 saved: {second_out}"
    );
    assert!(
        second_out.contains("cache-hit=true"),
        "and the action must report it as a hit, which is what a workflow branches on: {second_out}"
    );
    assert!(
        !second_out.contains("greenlit: installed"),
        "run 2 must start from the converged image, not reinstall: {second_out}"
    );
    assert!(
        second_out.contains("all checks passed"),
        "artifacts still cross the job boundary on the second run: {second_out}"
    );

    // The cache counter reaches the end-of-run breakdown. Note the name is
    // padded to ten columns, so an assertion on `"cache 1 hit(s)"` would fail
    // for reasons unrelated to the cache.
    assert!(
        second_err.contains("hit(s)"),
        "the run record reports cache activity: {second_err}"
    );

    // Nothing in either run leaks the run's own credentials.
    for output in [&first_out, &first_err, &second_out, &second_err] {
        assert!(
            !output.contains("ACTIONS_RUNTIME_TOKEN="),
            "the runtime token must never be echoed"
        );
    }
}
