//! Real-daemon, real-network smoke test: the `fixtures/actions-ci` workflow
//! runs green end to end through the compiled `litci` binary
//! (`PHASE-3-actions.md` exit criterion 1).
//!
//! This is the one place `actions/checkout` (self-satisfied), a real
//! marketplace `setup-*` action (pinned by full SHA, needing a real network
//! fetch and a real pinned-Node-runtime download), a local composite action,
//! a local Docker action, a local *and* a remote variable reference, and
//! `-s`/process-environment secret references all run together against the
//! real Docker daemon. Pre-run variable/secret collection is exercised
//! through the same scripted integration boundaries the rest of the suite
//! uses: `support::fake_github::FakeGitHub` (env-injected
//! `LITCI_TEST_GITHUB_API_BASE_URL`) stands in for the authenticated
//! repository-variable lookup, and `Sandbox::seed_auth_token` supplies the
//! stored token that lookup needs.
//!
//! That stored token is *also* what `litci run` hands the action-fetch
//! machinery for the real `actions/setup-node` tarball download (host-side
//! action fetching is not workflow-token injection — see
//! `ARCHITECTURE.md`'s "`GITHUB_TOKEN`'s reserved-name rule" entry — and
//! `crate::vars::remote`'s test-only base-url override only redirects the
//! *variables* lookup, never `greenlit-actions`' own resolver/fetcher, which
//! always talks to real `api.github.com`). A syntactically-fake token would
//! make that anonymous-eligible request into an authenticated-but-invalid
//! one, which GitHub 401s outright even for a public repository — so this
//! test seeds the *real*, already-authenticated `gh auth token` from this
//! machine (mirroring `litci auth --gh`'s own fallback) rather than a
//! placeholder string, and self-skips with a clear notice if `gh` is not
//! authenticated here.
//!
//! Opt-in, like `crates/greenlit-runtime/tests/actions_nodejs_live.rs`: a
//! green run downloads a real marketplace action tarball, a real ~30-50 MiB
//! Node.js distribution (via `setup-node`'s own toolcache logic), and the
//! two pinned ~100 MiB node20 runtime bundles (standard + Alpine — the
//! executor ensures both before it knows the job container's libc) into this
//! test's *own* isolated `$HOME`, which is not the developer's real
//! `~/.litci` cache and therefore cannot reuse it between runs. That is real
//! and expected (`greenlit-v0-spec.md` "Tech": one static host binary,
//! `AGENTS.md`: "network IS available" for this closeout), but it is too
//! slow/network-heavy for the default `cargo test --workspace` path, so it
//! only runs with `LITCI_TEST_LIVE_ACTIONS_CI=1` set. Run explicitly with:
//!
//! ```text
//! LITCI_TEST_LIVE_ACTIONS_CI=1 cargo test -p greenlit-app --test actions_ci_smoke -- --ignored
//! ```
//!
//! (also requires a reachable Docker daemon, internet access, and `gh auth
//! token` to already be authenticated on this machine).

pub mod support;

use std::path::{Path, PathBuf};

use support::Sandbox;
use support::fake_github::{Canned, FakeGitHub};

const LIVE_ENV_VAR: &str = "LITCI_TEST_LIVE_ACTIONS_CI";

fn fixture_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    std::fs::canonicalize(format!("{manifest}/../../fixtures/actions-ci"))
        .expect("fixtures/actions-ci exists")
}

/// Recursively copies the committed fixture tree into the sandbox's working
/// directory. `std::fs::copy` preserves the source's permission bits (the
/// Docker action's `entrypoint.sh` needs its executable bit to survive), so
/// no separate `chmod` step is needed.
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

/// Whether a Docker daemon answers right now — the same reachable-or-notice
/// pattern `crates/greenlit-runtime/tests/dockerkit` uses, reimplemented
/// locally: this crate's tests spawn the compiled binary rather than linking
/// the executor directly, so there is no shared `dockerkit` module to import
/// across crates.
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

/// The already-authenticated `gh` CLI token on this machine, mirroring
/// `litci auth --gh`'s own fallback — see this file's module doc comment for
/// why a real, working token is required here (not a placeholder string).
fn real_github_token() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!token.is_empty()).then_some(token)
}

#[test]
fn actions_ci_fixture_runs_green_end_to_end() {
    if std::env::var_os(LIVE_ENV_VAR).is_none() {
        eprintln!(
            "actions_ci_fixture_runs_green_end_to_end: skipped (set {LIVE_ENV_VAR}=1 to run the real-network, real-daemon actions-ci smoke test)"
        );
        return;
    }
    if !docker_daemon_reachable() {
        eprintln!("actions_ci_fixture_runs_green_end_to_end: no Docker daemon reachable; skipping");
        return;
    }
    let Some(token) = real_github_token() else {
        eprintln!(
            "actions_ci_fixture_runs_green_end_to_end: `gh auth token` is not available; skipping (see this file's module doc comment for why a real token is required)"
        );
        return;
    };

    let sandbox = Sandbox::new();
    copy_fixture_into(&fixture_root(), sandbox.root());
    sandbox.init_git();
    sandbox.seed_auth_token(&token);

    // Stands in for GitHub's own `GET /repos/{owner}/{repo}/actions/variables/{name}`
    // (`crate::vars::remote::VariablesClient::get_repo_variable`): the
    // workflow's `vars.REMOTE_GREETING` is not supplied locally, so `litci
    // run` must reach this exact endpoint through the authenticated lookup
    // chain before planning completes.
    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    let handle = server.serve(vec![Canned::json(
        200,
        "OK",
        r#"{"name":"REMOTE_GREETING","value":"greenlit-remote"}"#,
    )]);

    let output = sandbox.run_with_env(
        &[
            "run",
            "-W",
            ".github/workflows/ci.yml",
            "--var",
            "LOCAL_MODE=ci",
            "-s",
            "CLI_SECRET=cli-secret-value",
        ],
        &[
            ("LITCI_TEST_GITHUB_API_BASE_URL", base_url.as_str()),
            ("ENV_SECRET", "env-secret-value"),
        ],
    );
    let requested_paths = handle.join().expect("fake GitHub server thread");

    let stdout = support::stdout_text(&output);
    let stderr = support::stderr_text(&output);
    assert!(
        output.status.success(),
        "the whole run must be green\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // The remote variable lookup genuinely happened through the mocked
    // boundary, not a local shortcut.
    assert_eq!(requested_paths.len(), 1, "{requested_paths:?}");
    assert!(
        requested_paths[0].contains("/actions/variables/REMOTE_GREETING"),
        "{requested_paths:?}"
    );

    // Key semantics the exit criterion names: outputs propagated (the
    // composite's mapped output, checkout's outputs, and setup-node's own
    // output all feed the final verification step, which only prints this
    // line if every `test` in its script passed) and post ran (setup-node's
    // `post:` phase, rendered in the end-of-run table with its own sigil).
    assert!(
        stdout.contains("all checks passed"),
        "local/remote vars, secrets, and every action's outputs must all resolve correctly\n--- stdout ---\n{stdout}"
    );
    assert!(
        stdout.contains("Post actions/setup-node@8f152de45cc393bb48ce5d89d36b731f54556e65"),
        "setup-node's post phase must appear (and, since the whole run is green, have succeeded)\n--- stdout ---\n{stdout}"
    );
    assert!(
        stdout.contains("Post actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683"),
        "checkout's post phase (a documented no-op for self-checkout) must still appear\n--- stdout ---\n{stdout}"
    );

    // Neither secret value nor the real token ever appears in any output.
    assert!(!stdout.contains("cli-secret-value"));
    assert!(!stdout.contains("env-secret-value"));
    assert!(!stderr.contains("cli-secret-value"));
    assert!(!stderr.contains("env-secret-value"));
    assert!(!stdout.contains(&token));
    assert!(!stderr.contains(&token));
}
