//! Phase 12 containment for the committed `actions-ci` fixture.
//!
//! The fixture deliberately reaches actions, secrets, and a remote variable.
//! Source is captured privately before assessment. Until those paths are
//! certified, even `--allow-degraded` must then block before credentials,
//! GitHub traffic, action resolution, daemon startup, or engine work.

pub mod support;

use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use support::Sandbox;

fn fixture_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    std::fs::canonicalize(format!("{manifest}/../../fixtures/actions-ci"))
        .expect("fixtures/actions-ci exists")
}

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

#[test]
fn actions_ci_fixture_is_blocked_before_capability_side_effects() {
    const CLI_SECRET: &str = "actions-cli-secret-must-not-be-read-7391";
    const ENV_SECRET: &str = "actions-env-secret-must-not-be-read-7391";
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind GitHub API recording boundary");
    listener
        .set_nonblocking(true)
        .expect("make recording boundary nonblocking");
    let base_url = format!(
        "http://{}",
        listener.local_addr().expect("recording boundary address")
    );

    let sandbox = Sandbox::new();
    copy_fixture_into(&fixture_root(), sandbox.root());
    sandbox.init_git();

    let output = sandbox.run_with_env(
        &[
            "run",
            "-W",
            ".github/workflows/ci.yml",
            "--no-input",
            "--allow-degraded",
            "--var",
            "LOCAL_MODE=ci",
            "-s",
            &format!("CLI_SECRET={CLI_SECRET}"),
        ],
        &[
            ("LITCI_TEST_GITHUB_API_BASE_URL", base_url.as_str()),
            ("ENV_SECRET", ENV_SECRET),
            ("DOCKER_HOST", "ssh://example"),
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    let stdout = support::stdout_text(&output);
    let stderr = support::stderr_text(&output);
    assert!(
        ["secret.context", "variable.context", "action.uses"]
            .iter()
            .any(|capability| stderr.contains(&format!("uncertified capability `{capability}`"))),
        "the rich fixture did not report a non-forceable capability: {stderr}"
    );
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "actions-ci quarantine reached engine detection: {stderr}"
    );
    for sentinel in [CLI_SECRET, ENV_SECRET] {
        assert!(!stdout.contains(sentinel), "{stdout}");
        assert!(!stderr.contains(sentinel), "{stderr}");
    }
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Ok(_) => panic!("actions-ci quarantine contacted the GitHub API"),
        Err(error) => panic!("could not inspect the GitHub API recording boundary: {error}"),
    }
    assert!(
        !sandbox.home().join(".litci/daemon/v1.sock").exists(),
        "actions-ci quarantine started the daemon"
    );
    let mut runs = std::fs::read_dir(sandbox.home().join(".litci/runs"))
        .expect("actions-ci quarantine retained run evidence")
        .map(|entry| entry.expect("read retained run entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 1, "one invocation retained exactly one run");
    let run = runs.pop().expect("one retained run");
    assert!(
        run.join("source/.github/workflows/ci.yml").is_file(),
        "the retained run did not contain the captured actions-ci workflow"
    );
    let result: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run.join("result.json")).expect("read retained terminal result"),
    )
    .expect("parse retained terminal result");
    assert_eq!(result["conclusion"], "blocked");
    assert_eq!(result["compatibility"], "unsupported");
    assert_eq!(result["assurance"], "none");
}
