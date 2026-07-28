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

fn assert_value_absent_from_outputs_metrics_and_runs(
    sandbox: &Sandbox,
    output: &std::process::Output,
    sentinel: &str,
) {
    assert!(
        !output
            .stdout
            .windows(sentinel.len())
            .any(|w| w == sentinel.as_bytes())
    );
    assert!(
        !output
            .stderr
            .windows(sentinel.len())
            .any(|w| w == sentinel.as_bytes())
    );
    for root in [
        sandbox.home().join(".litci/metrics"),
        sandbox.home().join(".litci/runs"),
    ] {
        if !root.exists() {
            continue;
        }
        let mut pending = vec![root];
        while let Some(path) = pending.pop() {
            let metadata = std::fs::symlink_metadata(&path).expect("inspect retained path");
            if metadata.is_dir() {
                pending.extend(
                    std::fs::read_dir(&path)
                        .expect("walk retained directory")
                        .map(|entry| entry.expect("read retained entry").path()),
                );
            } else if metadata.is_file() {
                let bytes = std::fs::read(&path).expect("read retained file");
                assert!(
                    !bytes
                        .windows(sentinel.len())
                        .any(|window| window == sentinel.as_bytes()),
                    "credential-shaped value reached {}",
                    path.display()
                );
            }
        }
    }
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
    let expected_event_conclusion = match expected_conclusion {
        "blocked" => "Blocked",
        "preparation_failed" => "PreparationFailed",
        other => panic!("unmapped retained-result conclusion {other}"),
    };
    let expected_event_compatibility = match expected_compatibility {
        "unsupported" => "Unsupported",
        other => panic!("unmapped retained-result compatibility {other}"),
    };
    let terminal_events = std::fs::read_to_string(run.join("events.ndjson"))
        .expect("read retained terminal journal")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse run event"))
        .filter(|event| event["type"] == "run_finished")
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_events.len(),
        1,
        "retained journal must contain exactly one terminal event"
    );
    let terminal = &terminal_events[0];
    assert_eq!(terminal["conclusion"], expected_event_conclusion);
    assert_eq!(terminal["compatibility"], expected_event_compatibility);
    assert_eq!(terminal["assurance"], "None");
    assert_eq!(
        terminal["evidence"],
        run.file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 run id")
    );
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

fn assert_dispatch_inputs_are_nonforceable() {
    const SENTINEL: &str = "ghp_GL_STAB_DISPATCH_SENTINEL_027";
    let workflow = "\
on:
  workflow_dispatch:
    inputs:
      deployment_token:
        type: string
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ inputs.deployment_token }}
";

    for allow_degraded in [false, true] {
        let assignment = format!("deployment_token={SENTINEL}");
        let mut arguments = vec!["-e", "workflow_dispatch", "--input", assignment.as_str()];
        if allow_degraded {
            arguments.push("--allow-degraded");
        }
        let (sandbox, output) = run_workflow(workflow, &arguments);
        assert_eq!(output.status.code(), Some(1));
        let stderr = support::stderr_text(&output);
        assert!(
            stderr.contains("uncertified capability `input.dispatch`"),
            "{stderr}"
        );
        assert!(stderr.contains("stabilization Phase 16"), "{stderr}");
        assert!(!stderr.contains("DOCKER_HOST"), "{stderr}");
        assert!(
            !sandbox.home().join(".litci/runs").exists(),
            "explicit inputs reached source-evidence creation"
        );
        assert_value_absent_from_outputs_metrics_and_runs(&sandbox, &output, SENTINEL);

        let mut implicit_arguments = vec!["-e", "workflow_dispatch"];
        if allow_degraded {
            implicit_arguments.push("--allow-degraded");
        }
        let (sandbox, output) = run_workflow(workflow, &implicit_arguments);
        assert_eq!(output.status.code(), Some(1));
        let stderr = support::stderr_text(&output);
        assert!(
            stderr.contains("uncertified capability `input.dispatch`"),
            "{stderr}"
        );
        assert!(stderr.contains("stabilization Phase 16"), "{stderr}");
        assert!(!stderr.contains("DOCKER_HOST"), "{stderr}");
        assert_retained_result(&sandbox, workflow, "blocked", "unsupported");
    }
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
    assert_dispatch_inputs_are_nonforceable();
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
      - if: false
        run: echo ${{ vars.UNSELECTED }}
      - run: echo selected
  unselected:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: echo ${{ github.token }}
      - run: echo ${{ vars.UNSELECTED }}
";
    let (_sandbox, output) = run_workflow(workflow, &["--job", "selected", "--allow-degraded"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("forced: execution.shell"), "{stderr}");
    for protected in [
        "action.uses",
        "secret.context",
        "credential.github",
        "variable.context",
    ] {
        assert!(
            !stderr.contains(protected),
            "unreachable {protected} blocked selection: {stderr}"
        );
    }
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
}

#[test]
fn explicit_variables_are_nonforceable_before_host_source_or_engine_work() {
    const SENTINEL: &str = "github_pat_GL_STAB_EXPLICIT_VAR_SENTINEL_027";
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/ci.yml", SHELL_WORKFLOW);
    sandbox.write("source-sentinel.txt", SENTINEL);
    sandbox.init_git();
    let assignment = format!("UNUSED={SENTINEL}");
    let output = sandbox.run_with_env(
        &[
            "run",
            "--no-input",
            "--allow-degraded",
            "--var",
            assignment.as_str(),
        ],
        &[SSH_DOCKER_HOST],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("uncertified capability `variable.context`")
            && stderr.contains("command-line.--var")
            && stderr.contains("stabilization Phase 16"),
        "explicit variable did not hit the Phase 16 quarantine: {stderr}"
    );
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "explicit-variable quarantine reached host validation: {stderr}"
    );
    assert!(
        !sandbox.home().join(".litci/runs").exists(),
        "explicit-variable quarantine captured source evidence"
    );
    assert_value_absent_from_outputs_metrics_and_runs(&sandbox, &output, SENTINEL);
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
    let (_sandbox, output) = run_workflow(workflow, &["--allow-degraded"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("variable.context"),
        "a later unresolved-variable condition was incorrectly pruned: {stderr}"
    );
    assert!(!stderr.contains("DOCKER_HOST"), "{stderr}");
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
    const SENTINEL: &str = "github_pat_GL_STAB_VAR_SENTINEL_027";
    let literal_workflow = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ vars.DEPLOY_VALUE }}
";
    let dynamic_workflow = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ vars[github.event_name] }}
";

    for allow_degraded in [false, true] {
        let cli_assignment = format!("DEPLOY_VALUE={SENTINEL}");
        let mut cli_args = vec!["--var", cli_assignment.as_str()];
        if allow_degraded {
            cli_args.push("--allow-degraded");
        }
        let (sandbox, output) = run_workflow(literal_workflow, &cli_args);
        assert_vars_context_block(&sandbox, &output, SENTINEL);

        let sandbox = Sandbox::new();
        sandbox.write(".github/workflows/ci.yml", literal_workflow);
        sandbox.init_git();
        let mut env_args = vec!["run", "--no-input"];
        if allow_degraded {
            env_args.push("--allow-degraded");
        }
        let output =
            sandbox.run_with_env(&env_args, &[SSH_DOCKER_HOST, ("DEPLOY_VALUE", SENTINEL)]);
        assert_vars_context_block(&sandbox, &output, SENTINEL);

        let sandbox = Sandbox::new();
        sandbox.write(".github/workflows/ci.yml", dynamic_workflow);
        sandbox.write(".litci/vars", &format!("push={SENTINEL}\n"));
        sandbox.init_git();
        let mut dotenv_args = vec!["run", "--no-input"];
        if allow_degraded {
            dotenv_args.push("--allow-degraded");
        }
        let output = sandbox.run_with_env(&dotenv_args, &[SSH_DOCKER_HOST]);
        assert_vars_context_block(&sandbox, &output, SENTINEL);
    }
}

fn assert_vars_context_block(sandbox: &Sandbox, output: &std::process::Output, sentinel: &str) {
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(output);
    assert!(
        stderr.contains("uncertified capability `variable.context`"),
        "{stderr}"
    );
    assert!(stderr.contains("stabilization Phase 16"), "{stderr}");
    assert!(!stderr.contains("DOCKER_HOST"), "{stderr}");
    assert_value_absent_from_outputs_metrics_and_runs(sandbox, output, sentinel);
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
        "metric failure was not reported as a publication failure: {stderr}"
    );
    assert_eq!(
        stderr
            .matches("could not append the sanitized local run metric")
            .count(),
        1,
        "one failed append was reported more than once: {stderr}"
    );
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "quarantine reached engine detection: {stderr}"
    );
    let mut runs = std::fs::read_dir(sandbox.home().join(".litci/runs"))
        .expect("read retained unpublished run")
        .map(|entry| entry.expect("read retained run entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 1, "one invocation retained one run");
    let run = runs.pop().expect("one retained unpublished run");
    assert!(
        !run.join("result.json").exists(),
        "metrics failure published a terminal result"
    );
    let run_id = run
        .file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 run id");
    let store = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(sandbox.home()),
    )
    .expect("open content catalog");
    assert!(
        store
            .reclaimable_run_ids()
            .expect("read terminal catalog runs")
            .iter()
            .any(|candidate| candidate == run_id),
        "unpublished run was not marked aborted in the content catalog"
    );
}
