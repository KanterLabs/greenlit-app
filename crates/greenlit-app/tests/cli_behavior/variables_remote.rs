//! Compiled-CLI coverage for Phase 12 remote-variable containment.
//!
//! Remote repository and organization variables are temporarily
//! non-forceable. This recording boundary proves source capture happens first
//! and quarantine then wins before GitHub API traffic, daemon startup, and
//! engine detection.

use std::io::ErrorKind;
use std::net::TcpListener;

use super::common::LITERAL_VAR_WORKFLOW;
use super::support;
use super::support::Sandbox;

#[test]
fn remote_variable_is_blocked_before_network_or_engine_work() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind GitHub API recording boundary");
    listener
        .set_nonblocking(true)
        .expect("make recording boundary nonblocking");
    let base_url = format!(
        "http://{}",
        listener.local_addr().expect("recording boundary address")
    );

    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", LITERAL_VAR_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run_with_env(
        &["run", "-W", "wf.yml", "--no-input", "--allow-degraded"],
        &[
            ("LITCI_TEST_GITHUB_API_BASE_URL", base_url.as_str()),
            ("DOCKER_HOST", "ssh://example"),
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("uncertified capability `variable.remote`"),
        "{stderr}"
    );
    assert!(stderr.contains("vars.MODE"), "{stderr}");
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "remote-variable quarantine reached engine detection: {stderr}"
    );
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Ok(_) => panic!("remote-variable quarantine contacted the GitHub API"),
        Err(error) => panic!("could not inspect the GitHub API recording boundary: {error}"),
    }
    assert!(
        !sandbox.home().join(".litci/daemon/v1.sock").exists(),
        "remote-variable quarantine started the daemon"
    );
    let mut runs = std::fs::read_dir(sandbox.home().join(".litci/runs"))
        .expect("remote-variable quarantine retained run evidence")
        .map(|entry| entry.expect("read retained run entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 1, "one invocation retained exactly one run");
    let run = runs.pop().expect("one retained run");
    assert!(
        run.join("source/wf.yml").is_file(),
        "the retained run did not contain the captured workflow"
    );
    let result: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run.join("result.json")).expect("read retained terminal result"),
    )
    .expect("parse retained terminal result");
    assert_eq!(result["conclusion"], "blocked");
    assert_eq!(result["compatibility"], "unsupported");
    assert_eq!(result["assurance"], "none");
}
