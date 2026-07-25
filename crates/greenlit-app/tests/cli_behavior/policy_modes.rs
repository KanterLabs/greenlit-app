//! Real-daemon acceptance for clean and hermetic execution policy.

use std::path::Path;

use super::support;
use super::support::Sandbox;

const LIVE_ENV_VAR: &str = "LITCI_TEST_LIVE_FULL_CI";

fn docker_daemon_reachable() -> bool {
    use greenlit_runtime::{ContainerEngine, DockerEngine, Endpoint};

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

fn newest_run(home: &Path) -> std::path::PathBuf {
    let mut runs = std::fs::read_dir(home.join(".litci/runs"))
        .expect("run evidence directory exists")
        .map(|entry| entry.expect("run entry").path())
        .collect::<Vec<_>>();
    runs.sort();
    runs.pop().expect("at least one run was recorded")
}

#[test]
fn clean_and_hermetic_modes_change_runtime_state_and_evidence() {
    if std::env::var_os(LIVE_ENV_VAR).is_none() {
        eprintln!(
            "clean_and_hermetic_modes_change_runtime_state_and_evidence: skipped \
             (set {LIVE_ENV_VAR}=1 to run the real-daemon policy test)"
        );
        return;
    }
    assert!(
        docker_daemon_reachable(),
        "{LIVE_ENV_VAR} requires the owning test job to provide a reachable Docker daemon"
    );

    let sandbox = Sandbox::new();
    sandbox.write_home(".litci/toolcache/greenlit-clean-sentinel", "mutable");
    sandbox.write_home(
        ".litci/package-cache/cargo/registry/greenlit-package-sentinel",
        "download",
    );
    sandbox.write(
        ".github/workflows/ci.yml",
        "\
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - run: |
          test -f \"$RUNNER_TOOL_CACHE/greenlit-clean-sentinel\"
          test -f /usr/local/cargo/registry/greenlit-package-sentinel
",
    );
    sandbox.init_git();

    let ordinary = sandbox.run(&["run", "--no-input"]);
    assert!(
        ordinary.status.success(),
        "ordinary cache visibility run failed\nstdout:\n{}\nstderr:\n{}",
        support::stdout_text(&ordinary),
        support::stderr_text(&ordinary)
    );

    sandbox.write(
        ".github/workflows/ci.yml",
        "\
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - run: |
          test ! -f \"$RUNNER_TOOL_CACHE/greenlit-clean-sentinel\"
          test -f /usr/local/cargo/registry/greenlit-package-sentinel
",
    );
    sandbox.git(&["add", ".github/workflows/ci.yml"]);
    sandbox.git(&["commit", "-q", "-m", "clean policy"]);
    let clean = sandbox.run(&["run", "--clean", "--no-input"]);
    assert!(
        clean.status.success(),
        "clean run failed\nstdout:\n{}\nstderr:\n{}",
        support::stdout_text(&clean),
        support::stderr_text(&clean)
    );
    let clean_run = newest_run(sandbox.home());
    let clean_lock: serde_json::Value =
        serde_json::from_slice(&std::fs::read(clean_run.join("run-lock.json")).expect("lock"))
            .expect("lock JSON");
    let clean_result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(clean_run.join("result.json")).expect("result"))
            .expect("result JSON");
    assert_eq!(clean_lock["clean"], true);
    assert_eq!(clean_lock["hermetic"], false);
    assert_eq!(clean_result["assurance"], "clean");

    sandbox.write(
        ".github/workflows/ci.yml",
        "\
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - run: |
          if timeout 2 bash -c 'exec 3<>/dev/tcp/1.1.1.1/80'; then
            echo external traffic escaped the hermetic policy
            exit 1
          fi
",
    );
    sandbox.git(&["add", ".github/workflows/ci.yml"]);
    sandbox.git(&["commit", "-q", "-m", "hermetic policy"]);
    let hermetic = sandbox.run(&["run", "--hermetic", "--no-input"]);
    assert!(
        hermetic.status.success(),
        "hermetic run failed\nstdout:\n{}\nstderr:\n{}",
        support::stdout_text(&hermetic),
        support::stderr_text(&hermetic)
    );
    let hermetic_run = newest_run(sandbox.home());
    let hermetic_lock: serde_json::Value =
        serde_json::from_slice(&std::fs::read(hermetic_run.join("run-lock.json")).expect("lock"))
            .expect("lock JSON");
    assert_eq!(hermetic_lock["clean"], true);
    assert_eq!(hermetic_lock["hermetic"], true);
}
