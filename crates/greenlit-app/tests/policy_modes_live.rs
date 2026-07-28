//! Real-daemon acceptance for explicitly degraded clean and hermetic policy.

pub mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use support::Sandbox;

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

fn result_json(run: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(run.join("result.json")).expect("result"))
        .expect("result JSON")
}

fn assert_degraded_without_assurance(result: &serde_json::Value) {
    assert_eq!(result["conclusion"], "passed");
    assert_eq!(result["compatibility"], "degraded");
    assert_eq!(result["assurance"], "none");
}

fn assert_daemon_quarantined(sandbox: &Sandbox) {
    assert!(
        !sandbox.home().join(".litci/daemon").exists(),
        "a Phase 12 policy run started or prepared daemon state"
    );
}

#[test]
fn clean_and_hermetic_modes_preserve_runtime_policy_but_not_assurance() {
    assert!(
        docker_daemon_reachable(),
        "the owning clean/hermetic test job must provide a reachable Docker daemon"
    );

    let sandbox = Sandbox::new();
    sandbox.write_home(".litci/toolcache/greenlit-clean-sentinel", "mutable");
    sandbox.write_home(
        ".litci/package-cache/cargo/registry/greenlit-package-sentinel",
        "download",
    );
    std::fs::set_permissions(
        sandbox.home().join(".litci"),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("secure test state root");
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

    let ordinary = sandbox.run_with_env(&["run", "--no-input", "--allow-degraded"], &[]);
    assert!(
        ordinary.status.success(),
        "ordinary cache visibility run failed\nstdout:\n{}\nstderr:\n{}",
        support::stdout_text(&ordinary),
        support::stderr_text(&ordinary)
    );
    let ordinary_run = newest_run(sandbox.home());
    assert_degraded_without_assurance(&result_json(&ordinary_run));
    support::assert_run_resources_removed(&ordinary_run);
    assert_daemon_quarantined(&sandbox);

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
    let clean = sandbox.run_with_env(&["run", "--clean", "--no-input", "--allow-degraded"], &[]);
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
    assert_eq!(clean_lock["clean"], true);
    assert_eq!(clean_lock["hermetic"], false);
    assert_degraded_without_assurance(&result_json(&clean_run));
    support::assert_run_resources_removed(&clean_run);
    assert_daemon_quarantined(&sandbox);

    sandbox.write(
        ".github/workflows/ci.yml",
        "\
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - run: |
          network_state=blocked
          if timeout 5 bash -c 'exec 3<>/dev/tcp/1.1.1.1/80'; then
            network_state=reachable
          fi
          printf 'greenlit-network-control=%s\\n' \"$network_state\"
",
    );
    sandbox.git(&["add", ".github/workflows/ci.yml"]);
    sandbox.git(&["commit", "-q", "-m", "hermetic policy"]);

    // Run the exact committed workflow and endpoint without hermetic policy
    // first. This is the positive control that attributes the next run's
    // blocked connection to Greenlit rather than the CI host or endpoint.
    let network_control = sandbox.run_with_env(&["run", "--no-input", "--allow-degraded"], &[]);
    assert!(
        network_control.status.success(),
        "ordinary network positive control failed\nstdout:\n{}\nstderr:\n{}",
        support::stdout_text(&network_control),
        support::stderr_text(&network_control)
    );
    let network_control_run = newest_run(sandbox.home());
    let network_control_events = std::fs::read_to_string(network_control_run.join("events.ndjson"))
        .expect("ordinary network-control journal");
    assert!(
        network_control_events.contains("greenlit-network-control=reachable"),
        "the ordinary run could not reach the hermetic test endpoint"
    );
    assert_degraded_without_assurance(&result_json(&network_control_run));
    support::assert_run_resources_removed(&network_control_run);
    assert_daemon_quarantined(&sandbox);

    let hermetic = sandbox.run_with_env(
        &["run", "--hermetic", "--no-input", "--allow-degraded"],
        &[],
    );
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
    let hermetic_events = std::fs::read_to_string(hermetic_run.join("events.ndjson"))
        .expect("hermetic network-control journal");
    assert!(
        hermetic_events.contains("greenlit-network-control=blocked"),
        "the hermetic run reached an endpoint proven reachable by the ordinary control"
    );
    assert!(
        !hermetic_events.contains("greenlit-network-control=reachable"),
        "external traffic escaped the hermetic policy"
    );
    assert_eq!(hermetic_lock["clean"], true);
    assert_eq!(hermetic_lock["hermetic"], true);
    assert_degraded_without_assurance(&result_json(&hermetic_run));
    support::assert_run_resources_removed(&hermetic_run);
    assert_daemon_quarantined(&sandbox);
}
