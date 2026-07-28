//! Phase 12 containment for the committed `full-ci` fixture.
//!
//! The fixture reaches services and marketplace actions. Those capabilities
//! are non-forceable until their owning stabilization phases certify them.
//! The compiled CLI captures source privately, then stops before daemon,
//! action, service, cache, artifact, or engine side effects.

pub mod support;

use std::path::{Path, PathBuf};

use support::Sandbox;

fn fixture_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    std::fs::canonicalize(format!("{manifest}/../../fixtures/full-ci"))
        .expect("fixtures/full-ci exists")
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
fn full_ci_fixture_is_blocked_before_service_and_action_side_effects() {
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
        ],
        &[("DOCKER_HOST", "ssh://example")],
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(
        ["action.uses", "service.container"]
            .iter()
            .any(|capability| stderr.contains(&format!("uncertified capability `{capability}`"))),
        "the rich fixture did not report its non-forceable boundary: {stderr}"
    );
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "full-ci quarantine reached engine detection: {stderr}"
    );
    assert!(
        !sandbox.home().join(".litci/daemon/v1.sock").exists(),
        "full-ci quarantine started the daemon"
    );
    let mut runs = std::fs::read_dir(sandbox.home().join(".litci/runs"))
        .expect("full-ci quarantine retained run evidence")
        .map(|entry| entry.expect("read retained run entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 1, "one invocation retained exactly one run");
    let run = runs.pop().expect("one retained run");
    assert!(
        run.join("source/.github/workflows/ci.yml").is_file(),
        "the retained run did not contain the captured full-ci workflow"
    );
    let result: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run.join("result.json")).expect("read retained terminal result"),
    )
    .expect("parse retained terminal result");
    assert_eq!(result["conclusion"], "blocked");
    assert_eq!(result["compatibility"], "unsupported");
    assert_eq!(result["assurance"], "none");
    assert!(
        !sandbox.home().join(".litci/actions").exists(),
        "full-ci quarantine created action-resolution state"
    );
}
