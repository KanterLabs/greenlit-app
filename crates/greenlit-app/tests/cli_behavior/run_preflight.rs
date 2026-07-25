//! `litci run` rejects a malformed `uses:` reference before any engine
//! work; a syntactically valid one now proceeds to resolution/execution
//! (`PHASE-3-actions.md` "Action execution" narrowed the Phase 2 blanket
//! rejection — see `greenlit_runtime::executor::preflight`'s module docs).
//!
//! The oracle for "preflight passed" is deliberate: every test sets
//! `DOCKER_HOST=ssh://example`, which engine detection rejects with a known
//! message in any environment, daemon or not. A run that fails with the
//! DOCKER_HOST error therefore got past preflight; a run that fails with
//! the malformed-`uses:` error never touched the engine.

use super::support;
use super::support::Sandbox;

const SSH_DOCKER_HOST: (&str, &str) = ("DOCKER_HOST", "ssh://example");

const MALFORMED_USES_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: not-a-valid-reference
";

const WELL_FORMED_USES_WORKFLOW: &str = "\
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
      - uses: not-a-valid-reference
";

const SKIPPED_MALFORMED_USES_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - if: false
        uses: not-a-valid-reference
      - run: echo hi
";

const MATRIX_MALFORMED_USES_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        leg: [a, b]
    steps:
      - uses: not-a-valid-reference
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
fn a_malformed_uses_reference_is_rejected_before_any_engine_work() {
    let output = run_workflow(MALFORMED_USES_WORKFLOW, &[]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("`uses: not-a-valid-reference`"), "{stderr}");
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
fn a_well_formed_uses_reference_passes_preflight_and_reaches_engine_detection() {
    // `actions/checkout@v4` parses into one of GitHub's four documented
    // `uses:` forms, so preflight (which only rejects a malformed
    // reference) lets it through; the run then fails at engine detection,
    // proving preflight never rejected it.
    let output = run_workflow(WELL_FORMED_USES_WORKFLOW, &[]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("DOCKER_HOST"),
        "a syntactically valid uses: reference must reach engine detection: {stderr}"
    );
}

#[test]
fn offline_resolution_names_the_exact_missing_action_ref_before_engine_work() {
    let output = run_workflow(WELL_FORMED_USES_WORKFLOW, &["--offline", "--no-input"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("offline content is missing: action ref actions/checkout@v4"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "offline preparation must fail at the exact absent identity without trying another source: {stderr}"
    );
}

#[test]
fn a_matrix_leg_malformed_uses_step_is_rejected_before_any_engine_work() {
    let output = run_workflow(MATRIX_MALFORMED_USES_WORKFLOW, &[]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("`uses: not-a-valid-reference`"), "{stderr}");
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
    assert!(!stderr.contains("not-a-valid-reference"), "{stderr}");
}

#[test]
fn a_statically_skipped_malformed_uses_step_stays_accepted() {
    let output = run_workflow(SKIPPED_MALFORMED_USES_WORKFLOW, &[]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("DOCKER_HOST"),
        "an `if: false` uses: step never runs, so preflight must not reject it: {stderr}"
    );
    assert!(!stderr.contains("not-a-valid-reference"), "{stderr}");
}

#[test]
fn unsupported_preflight_persists_terminal_result_and_trace() {
    let sandbox = Sandbox::new();
    sandbox.write(
        ".github/workflows/ci.yml",
        "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    environment: production
    steps:
      - run: echo blocked
",
    );
    sandbox.init_git();

    let output = sandbox.run(&["run", "--no-input"]);
    assert!(!output.status.success());
    let runs = sandbox.home().join(".litci/runs");
    let run = std::fs::read_dir(&runs)
        .expect("run directory exists")
        .next()
        .expect("one run is persisted")
        .expect("run entry is readable")
        .path();
    let result: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run.join("result.json")).expect("terminal result is persisted"),
    )
    .expect("terminal result is JSON");
    assert_eq!(result["conclusion"], "preparation_failed");
    assert_eq!(result["compatibility"], "unsupported");
    assert_eq!(result["assurance"], "none");

    let trace =
        std::fs::read_to_string(run.join("trace.ndjson")).expect("append-only trace is persisted");
    assert!(trace.contains("\"event\":\"source_locked\""), "{trace}");
    assert!(
        trace.contains("\"event\":\"compatibility_analyzed\""),
        "{trace}"
    );
    assert!(trace.contains("\"event\":\"run_completed\""), "{trace}");
}
