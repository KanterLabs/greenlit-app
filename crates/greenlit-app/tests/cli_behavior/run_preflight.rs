//! Compiled-CLI coverage for the Phase 12 stabilization quarantine.
//!
//! `DOCKER_HOST=ssh://example` is the recording boundary for engine access:
//! the runtime rejects that non-local endpoint before contacting it. Seeing
//! the endpoint diagnostic proves a forceable shell-only run crossed the
//! quarantine; every hard-block case must fail without reaching it.

use super::support::{self, Sandbox};
use std::os::unix::fs::PermissionsExt;

const SSH_DOCKER_HOST: (&str, &str) = ("DOCKER_HOST", "ssh://example");

const SHELL_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo selected
";

fn run_workflow(workflow: &str, args: &[&str]) -> (Sandbox, std::process::Output) {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/ci.yml", workflow);
    sandbox.init_git();
    let mut full_args = vec!["run", "--no-input"];
    full_args.extend_from_slice(args);
    let output = sandbox.run_with_env(&full_args, &[SSH_DOCKER_HOST]);
    (sandbox, output)
}

fn assert_retained_result(
    sandbox: &Sandbox,
    workflow: &str,
    expected_conclusion: &str,
    expected_compatibility: &str,
) {
    let mut runs = std::fs::read_dir(sandbox.home().join(".litci/runs"))
        .expect("one invocation retained run evidence")
        .map(|entry| entry.expect("read retained run entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 1, "one invocation retained exactly one run");
    let run = runs.pop().expect("one retained run");
    assert_eq!(
        std::fs::read_to_string(run.join("source/.github/workflows/ci.yml"))
            .expect("read captured workflow source"),
        workflow,
        "retained run did not contain the captured workflow source"
    );
    let result: serde_json::Value = serde_json::from_slice(
        &std::fs::read(run.join("result.json")).expect("read retained terminal result"),
    )
    .expect("parse retained terminal result");
    assert_eq!(result["conclusion"], expected_conclusion);
    assert_eq!(result["compatibility"], expected_compatibility);
    assert_eq!(result["assurance"], "none");
    assert!(
        !sandbox.home().join(".litci/daemon").exists(),
        "preflight started or prepared daemon state"
    );
}

#[test]
fn shell_execution_is_blocked_by_default_and_explicitly_degraded_before_engine_work() {
    let blocked_sandbox = Sandbox::new();
    blocked_sandbox.write(".github/workflows/ci.yml", SHELL_WORKFLOW);
    blocked_sandbox.init_git();
    let blocked = blocked_sandbox.run_with_env(&["run", "--no-input"], &[SSH_DOCKER_HOST]);
    assert_eq!(blocked.status.code(), Some(1));
    let blocked_stderr = support::stderr_text(&blocked);
    assert!(
        blocked_stderr.contains("uncertified capability `execution.shell`"),
        "{blocked_stderr}"
    );
    assert!(
        blocked_stderr.contains("rerun with `--allow-degraded`"),
        "{blocked_stderr}"
    );
    assert!(!blocked_stderr.contains("DOCKER_HOST"), "{blocked_stderr}");
    assert_retained_result(&blocked_sandbox, SHELL_WORKFLOW, "blocked", "unsupported");

    let (_forced_sandbox, forced) = run_workflow(SHELL_WORKFLOW, &["--allow-degraded"]);
    assert_eq!(forced.status.code(), Some(1));
    let forced_stderr = support::stderr_text(&forced);
    assert!(
        forced_stderr.contains("`--allow-degraded` forced 1 uncertified capability"),
        "{forced_stderr}"
    );
    assert!(
        forced_stderr.contains("assurance is none"),
        "{forced_stderr}"
    );
    assert!(
        forced_stderr.contains("DOCKER_HOST"),
        "the explicit force did not reach the engine boundary: {forced_stderr}"
    );
}

#[test]
fn security_sensitive_capabilities_cannot_be_forced() {
    let cases = [
        (
            "secret",
            "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ secrets.DEPLOY_TOKEN }}
",
            "secret.context",
        ),
        (
            "github credential",
            "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ github.token }}
",
            "credential.github",
        ),
        (
            "remote variable",
            "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ vars.REMOTE_ONLY }}
",
            "variable.remote",
        ),
        (
            "action",
            "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
",
            "action.uses",
        ),
        (
            "service",
            "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    services:
      database:
        image: postgres:17
    steps:
      - run: echo ready
",
            "service.container",
        ),
    ];

    for (name, workflow, capability) in cases {
        let (sandbox, output) = run_workflow(workflow, &["--allow-degraded"]);
        assert_eq!(output.status.code(), Some(1), "{name}");
        let stderr = support::stderr_text(&output);
        assert!(
            stderr.contains(&format!("uncertified capability `{capability}`")),
            "{name}: {stderr}"
        );
        assert!(
            !stderr.contains("DOCKER_HOST"),
            "{name} reached engine detection: {stderr}"
        );
        assert_retained_result(&sandbox, workflow, "blocked", "unsupported");
        assert!(
            sandbox.home().join(".litci/metrics/runs.ndjson").is_file(),
            "{name} did not append the explicitly permitted sanitized metric"
        );
    }
}

#[test]
fn docker_text_and_commands_are_shell_degradation_not_inferred_dind() {
    for script in ["echo docker", "docker version"] {
        let workflow = format!(
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: {script}\n"
        );
        let (_sandbox, output) = run_workflow(&workflow, &["--allow-degraded"]);
        assert_eq!(output.status.code(), Some(1), "{script}");
        let stderr = support::stderr_text(&output);
        assert!(
            stderr.contains("forced: execution.shell"),
            "{script}: {stderr}"
        );
        assert!(
            !stderr.contains("infrastructure.dind"),
            "{script} was misclassified as DinD: {stderr}"
        );
        assert!(
            stderr.contains("DOCKER_HOST"),
            "{script} did not cross the engine boundary: {stderr}"
        );
    }
}

#[test]
fn selected_and_statically_unreachable_protected_capabilities_do_not_block() {
    let workflow = "\
on: push
jobs:
  selected:
    runs-on: ubuntu-latest
    steps:
      - if: false
        uses: actions/checkout@v4
      - if: false
        run: echo ${{ secrets.UNREACHABLE }}
      - run: echo selected
  unselected:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: echo ${{ github.token }}
";
    let (_sandbox, output) = run_workflow(workflow, &["--job", "selected", "--allow-degraded"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("forced: execution.shell"), "{stderr}");
    for protected in ["action.uses", "secret.context", "credential.github"] {
        assert!(
            !stderr.contains(protected),
            "unreachable {protected} blocked selection: {stderr}"
        );
    }
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
}

#[test]
fn exact_matrix_selection_uses_only_the_selected_legs_reachability() {
    let workflow = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        mode: [safe, protected]
    steps:
      - if: matrix.mode == 'protected'
        uses: actions/checkout@v4
      - run: echo ${{ matrix.mode }}
";
    let (_sandbox, output) = run_workflow(
        workflow,
        &[
            "--job",
            "build",
            "--matrix",
            "mode=safe",
            "--allow-degraded",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(!stderr.contains("action.uses"), "{stderr}");
    assert!(stderr.contains("forced: execution.shell"), "{stderr}");
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
}

#[test]
fn ambiguous_reachability_fails_closed_before_the_engine() {
    let workflow = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - if: success()
        run: echo maybe
";
    let (sandbox, output) = run_workflow(workflow, &["--allow-degraded"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("reachability.ambiguous"),
        "ambiguous condition was not quarantined: {stderr}"
    );
    assert!(!stderr.contains("DOCKER_HOST"), "{stderr}");
    assert_retained_result(&sandbox, workflow, "blocked", "unsupported");
}

#[test]
fn every_unresolved_variable_condition_remains_conservatively_reachable() {
    let workflow = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - if: false
        run: echo ${{ vars.REMOTE_FLAG }}
      - if: vars.REMOTE_FLAG == 'enabled'
        uses: actions/checkout@v4
";
    let (sandbox, output) = run_workflow(workflow, &["--allow-degraded"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("variable.remote"),
        "a later unresolved-variable condition was incorrectly pruned: {stderr}"
    );
    assert!(!stderr.contains("DOCKER_HOST"), "{stderr}");
    assert_retained_result(&sandbox, workflow, "blocked", "unsupported");
}

#[test]
fn hidden_daemon_lifecycle_commands_are_quarantined_before_socket_or_network_work() {
    let sandbox = support::Sandbox::new();
    for arguments in [
        &["daemon"][..],
        &["daemon", "--status"][..],
        &["daemon", "--shutdown"][..],
    ] {
        let output = sandbox.run_with_env(arguments, &[]);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        let stderr = support::stderr_text(&output);
        assert!(
            stderr.contains("disabled until Phase 25"),
            "{arguments:?}: {stderr}"
        );
        assert!(
            !sandbox.home().join(".litci/daemon").exists(),
            "{arguments:?} created daemon state"
        );
    }
}

#[test]
fn a_literal_local_variable_does_not_trigger_remote_credential_work() {
    let workflow = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ vars.LOCAL_ONLY }}
";
    let (_sandbox, output) =
        run_workflow(workflow, &["--var", "LOCAL_ONLY=value", "--allow-degraded"]);
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(!stderr.contains("variable.remote"), "{stderr}");
}

#[test]
fn authored_out_of_scope_behavior_is_rejected_after_source_capture_before_external_work() {
    let workflow = "\
on: push
jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: production
    steps:
      - run: echo blocked
";
    let (sandbox, output) = run_workflow(workflow, &["--allow-degraded"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("environment"), "{stderr}");
    assert!(stderr.contains("out of scope"), "{stderr}");
    assert!(!stderr.contains("DOCKER_HOST"), "{stderr}");
    assert_retained_result(&sandbox, workflow, "preparation_failed", "unsupported");
}

#[test]
fn write_back_is_a_distinct_nonforceable_capability_before_host_or_engine_mutation() {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/ci.yml", SHELL_WORKFLOW);
    sandbox.write("host-sentinel.txt", "host bytes must stay unchanged\n");
    sandbox.init_git();
    let before = std::fs::read(sandbox.root().join("host-sentinel.txt"))
        .expect("read host sentinel before quarantine");

    let output = sandbox.run_with_env(
        &["run", "--write-back", "--job", "build", "--allow-degraded"],
        &[SSH_DOCKER_HOST],
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("uncertified capability `source.write-back`"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("source.containment"),
        "write-back was collapsed into snapshot containment: {stderr}"
    );
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "write-back quarantine reached engine detection: {stderr}"
    );
    assert_retained_result(&sandbox, SHELL_WORKFLOW, "blocked", "unsupported");
    assert!(
        !sandbox.root().join(".litci").exists(),
        "write-back quarantine mutated repository-local state"
    );
    assert_eq!(
        std::fs::read(sandbox.root().join("host-sentinel.txt"))
            .expect("read host sentinel after quarantine"),
        before,
        "write-back quarantine mutated the host worktree"
    );
}

#[test]
fn metric_failure_cannot_replace_the_primary_quarantine_diagnostic() {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/ci.yml", SHELL_WORKFLOW);
    sandbox.init_git();
    let litci = sandbox.home().join(".litci");
    let metrics = litci.join("metrics");
    let metric_file_collision = metrics.join("runs.ndjson");
    std::fs::create_dir_all(&metric_file_collision).expect("create metric-file collision");
    for path in [&litci, &metrics, &metric_file_collision] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("secure metric collision path");
    }

    let output = sandbox.run_with_env(&["run"], &[SSH_DOCKER_HOST]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("uncertified capability `execution.shell`"),
        "metric failure replaced quarantine: {stderr}"
    );
    assert!(
        stderr.contains("could not append the sanitized local run metric"),
        "metric failure was not reported as a secondary warning: {stderr}"
    );
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "quarantine reached engine detection: {stderr}"
    );
    assert_retained_result(&sandbox, SHELL_WORKFLOW, "blocked", "unsupported");
}
