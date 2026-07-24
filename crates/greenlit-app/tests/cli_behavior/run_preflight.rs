//! `litci run` rejects definitely-executing `uses:` steps before any engine
//! work.
//!
//! The oracle for "preflight passed" is deliberate: every test sets
//! `DOCKER_HOST=ssh://example`, which engine detection rejects with a known
//! message in any environment, daemon or not. A run that fails with the
//! DOCKER_HOST error therefore got past preflight; a run that fails with the
//! Phase-3 `uses:` error never touched the engine.

use super::support;
use super::support::Sandbox;

const SSH_DOCKER_HOST: (&str, &str) = ("DOCKER_HOST", "ssh://example");

const USES_ONLY_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
";

const MIXED_WORKFLOW: &str = "\
on: push
jobs:
  shell:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
  action:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
";

const SKIPPED_USES_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - if: false
        uses: actions/checkout@v4
      - run: echo hi
";

const MATRIX_USES_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        leg: [a, b]
    steps:
      - uses: actions/checkout@v4
";

fn run_workflow(workflow: &str, args: &[&str]) -> std::process::Output {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/ci.yml", workflow);
    sandbox.init_git();
    let mut full_args = vec!["run"];
    full_args.extend_from_slice(args);
    sandbox.run_with_env(&full_args, &[SSH_DOCKER_HOST])
}

#[test]
fn an_unconditional_uses_step_is_rejected_before_any_engine_work() {
    let output = run_workflow(USES_ONLY_WORKFLOW, &[]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("`uses: actions/checkout@v4` is an action step"),
        "{stderr}"
    );
    assert!(stderr.contains("Phase 3"), "{stderr}");
    assert!(
        stderr.contains(".github/workflows/ci.yml:"),
        "span must locate the authored uses: value: {stderr}"
    );
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "rejection must precede engine detection: {stderr}"
    );
}

#[test]
fn a_matrix_leg_uses_step_is_rejected_before_any_engine_work() {
    let output = run_workflow(MATRIX_USES_WORKFLOW, &[]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("is an action step"), "{stderr}");
    assert!(!stderr.contains("DOCKER_HOST"), "{stderr}");
}

#[test]
fn pruning_to_a_shell_job_lets_a_mixed_workflow_past_preflight() {
    let output = run_workflow(MIXED_WORKFLOW, &["-j", "shell"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("DOCKER_HOST"),
        "the pruned plan has no uses: step, so the run must reach engine detection: {stderr}"
    );
    assert!(!stderr.contains("is an action step"), "{stderr}");
}

#[test]
fn a_statically_skipped_uses_step_stays_accepted() {
    let output = run_workflow(SKIPPED_USES_WORKFLOW, &[]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("DOCKER_HOST"),
        "an `if: false` uses: step never runs, so preflight must not reject it: {stderr}"
    );
    assert!(!stderr.contains("is an action step"), "{stderr}");
}
