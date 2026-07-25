//! `fixtures/full-ci` run warm and then offline, end to end, against a real
//! daemon.
//!
//! `docs/PHASE-4-environment.md` exit criterion 1: "workflow with a postgres
//! service, `actions/cache` (miss → save → hit across two runs), artifact
//! upload + download across jobs, and a tool supplied by the immutable runner
//! profile — `litci run` green twice, then from verified local setup content
//! with `--offline`."
//!
//! # Why twice, and why one sandbox
//!
//! Every claim here is about state *surviving between runs*. A cache that
//! saves is worth nothing if the next run does not restore it, and verified
//! immutable setup content is incomplete if the third run cannot replay it
//! offline. `Sandbox` gives
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

fn persisted_results(home: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let root = home.join(".litci/runs");
    let mut results = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(root).expect("read persisted runs") {
        let entry = entry.expect("run directory entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        let result = std::fs::read(entry.path().join("result.json"))
            .expect("completed run retains result evidence");
        results.insert(name, result);
    }
    results
}

#[test]
fn full_ci_fixture_is_green_warm_and_replays_verified_setup_offline() {
    if std::env::var_os(LIVE_ENV_VAR).is_none() {
        eprintln!(
            "full_ci_fixture_is_green_warm_and_replays_verified_setup_offline: skipped \
             (set {LIVE_ENV_VAR}=1 to run the real-daemon full-ci smoke test)"
        );
        return;
    }
    if !docker_daemon_reachable() {
        eprintln!(
            "full_ci_fixture_is_green_warm_and_replays_verified_setup_offline: \
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
        first_out.contains("profile tool ok"),
        "the locked runner profile must expose its declared toolset immediately: {first_out}"
    );
    assert!(
        first_out.contains("all checks passed"),
        "the dependent job's artifact check must run: {first_out}"
    );
    let cold_results = persisted_results(sandbox.home());
    assert_eq!(cold_results.len(), 1);

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
    assert!(!second_out.contains("greenlit: installing"), "{second_out}");
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
    let warm_results = persisted_results(sandbox.home());
    let warm_result = warm_results
        .iter()
        .find(|(run, _)| !cold_results.contains_key(*run))
        .map(|(_, result)| result)
        .expect("the warm run persisted distinct evidence");
    assert_eq!(
        warm_result,
        cold_results.values().next().expect("cold result"),
        "cache warmth may change performance evidence, never semantic result evidence"
    );

    // ---- Run 3: exact verified setup content only ----
    let offline = sandbox.run(&["run", "--offline", "--no-input"]);
    let offline_out = support::stdout_text(&offline);
    let offline_err = support::stderr_text(&offline);
    assert!(
        offline.status.success(),
        "fully cached offline run failed\nstdout:\n{offline_out}\nstderr:\n{offline_err}"
    );
    assert!(
        offline_err.contains("(CAS hit)"),
        "offline preparation must report verified CAS reuse: {offline_err}"
    );
    assert!(
        !offline_err.contains("pulling "),
        "offline preparation must not start an image download: {offline_err}"
    );

    // Nothing in any run leaks the run's own credentials.
    for output in [
        &first_out,
        &first_err,
        &second_out,
        &second_err,
        &offline_out,
        &offline_err,
    ] {
        assert!(
            !output.contains("ACTIONS_RUNTIME_TOKEN="),
            "the runtime token must never be echoed"
        );
    }

    // Exact matrix selection is also an evidence boundary: unselected legs
    // must not acquire JobLocks that imply they were resolved or executed.
    sandbox.write(
        ".github/workflows/ci.yml",
        "\
name: selected-matrix
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [ubuntu-24.04, ubuntu-22.04]
        node: [20, 22]
    steps:
      - run: echo selected
",
    );
    let selected = sandbox.run(&[
        "run",
        "--job",
        "build",
        "--matrix",
        "os=ubuntu-24.04",
        "--matrix",
        "node=20",
        "--no-input",
    ]);
    assert!(
        selected.status.success(),
        "selected matrix run failed\nstdout:\n{}\nstderr:\n{}",
        support::stdout_text(&selected),
        support::stderr_text(&selected)
    );
    let run = std::fs::read_dir(sandbox.home().join(".litci/runs"))
        .expect("read matrix run evidence")
        .map(|entry| entry.expect("matrix run entry").path())
        .max()
        .expect("matrix run persisted");
    let locks = std::fs::read_dir(run.join("job-locks"))
        .expect("selected job locks persisted")
        .collect::<Result<Vec<_>, _>>()
        .expect("selected job locks readable");
    assert_eq!(
        locks.len(),
        1,
        "only the selected matrix case receives a JobLock"
    );
    let lock: serde_json::Value =
        serde_json::from_slice(&std::fs::read(locks[0].path()).expect("selected JobLock readable"))
            .expect("selected JobLock is JSON");
    assert_eq!(
        lock["matrix"],
        serde_json::json!({
            "node": {"kind": "number", "value": 20.0},
            "os": {"kind": "string", "value": "ubuntu-24.04"}
        })
    );
}
