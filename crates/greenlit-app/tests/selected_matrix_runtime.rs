//! Real-daemon compiled-CLI acceptance for exact matrix selection evidence.

pub mod support;

use support::Sandbox;

#[test]
fn selected_matrix_shell_run_persists_only_the_selected_job_lock() {
    let sandbox = Sandbox::new();
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
    sandbox.init_git();

    let selected = sandbox.run(&[
        "run",
        "-W",
        ".github/workflows/ci.yml",
        "--job",
        "build",
        "--matrix",
        "os=ubuntu-24.04",
        "--matrix",
        "node=20",
        "--no-input",
        "--allow-degraded",
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
        .next()
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
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(run.join("result.json")).expect("result"))
            .expect("result JSON");
    assert_eq!(result["conclusion"], "passed");
    assert_eq!(result["compatibility"], "degraded");
    assert_eq!(result["assurance"], "none");
    support::assert_run_resources_removed(&run);
}
