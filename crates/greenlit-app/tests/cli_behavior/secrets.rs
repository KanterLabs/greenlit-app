//! Compiled-CLI coverage for Phase 12 secret containment.
//!
//! A reachable `secrets.*` reference is non-forceable. Source is captured
//! privately before assessment; quarantine must then win over local-input
//! collection, legacy-secret migration, daemon startup, and engine detection.
//! The dedicated credential-capability target owns successful persistent-
//! keyring behavior.

use super::support;
use super::support::Sandbox;

const SSH_DOCKER_HOST: (&str, &str) = ("DOCKER_HOST", "ssh://example");

const SECRET_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      TOKEN: ${{ secrets.API_TOKEN }}
    steps:
      - run: echo hi
";

const GITHUB_TOKEN_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      TOKEN: ${{ secrets.GITHUB_TOKEN }}
    steps:
      - run: echo hi
";

fn sandbox_with(workflow: &str) -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", workflow);
    sandbox.init_git();
    sandbox
}

fn assert_captured_blocked_run(sandbox: &Sandbox) {
    let mut runs = std::fs::read_dir(sandbox.home().join(".litci/runs"))
        .expect("secret quarantine retained run evidence")
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

fn assert_secret_quarantine(sandbox: &Sandbox, output: &std::process::Output, name: &str) {
    assert_eq!(output.status.code(), Some(1));
    let stderr = support::stderr_text(output);
    assert!(
        stderr.contains("uncertified capability `secret.context`"),
        "{stderr}"
    );
    assert!(stderr.contains(&format!("secrets.{name}")), "{stderr}");
    assert!(
        !stderr.contains("DOCKER_HOST"),
        "secret quarantine reached engine detection: {stderr}"
    );
    assert!(
        !sandbox.home().join(".litci/daemon/v1.sock").exists(),
        "secret quarantine started the daemon"
    );
    assert_captured_blocked_run(sandbox);
}

#[test]
fn ordinary_secret_is_blocked_before_input_migration_or_engine_work() {
    const SENTINEL: &str = "legacy-secret-must-not-be-read-7391";
    let sandbox = sandbox_with(SECRET_WORKFLOW);
    sandbox.write(".litci/secrets", &format!("API_TOKEN={SENTINEL}\n"));

    let output = sandbox.run_with_env(
        &["run", "-W", "wf.yml", "--allow-degraded"],
        &[SSH_DOCKER_HOST],
    );
    assert_secret_quarantine(&sandbox, &output, "API_TOKEN");

    let stdout = support::stdout_text(&output);
    let stderr = support::stderr_text(&output);
    assert!(!stdout.contains(SENTINEL), "{stdout}");
    assert!(!stderr.contains(SENTINEL), "{stderr}");
    assert!(
        sandbox.root().join(".litci/secrets").exists(),
        "quarantine read and migrated the legacy secret file"
    );
    assert!(
        !sandbox.root().join(".litci/secrets.vault").exists(),
        "quarantine created a secret vault"
    );
    assert!(
        !sandbox.home().join(".litci/vault.key").exists(),
        "quarantine created secret key material"
    );
}

#[test]
fn github_token_is_blocked_before_cli_credentials_are_used() {
    const CLI_SENTINEL: &str = "cli-token-must-not-be-read-7391";
    let sandbox = sandbox_with(GITHUB_TOKEN_WORKFLOW);

    let output = sandbox.run_with_env(
        &[
            "run",
            "-W",
            "wf.yml",
            "--no-input",
            "--allow-degraded",
            "-s",
            &format!("GITHUB_TOKEN={CLI_SENTINEL}"),
        ],
        &[SSH_DOCKER_HOST],
    );
    assert_secret_quarantine(&sandbox, &output, "GITHUB_TOKEN");

    let stdout = support::stdout_text(&output);
    let stderr = support::stderr_text(&output);
    assert!(!stdout.contains(CLI_SENTINEL), "{stdout}");
    assert!(!stderr.contains(CLI_SENTINEL), "{stderr}");
    assert!(
        !stderr.contains("cannot narrow"),
        "credential permissions were inspected before quarantine: {stderr}"
    );
}

#[test]
fn computed_dynamic_wildcard_and_bare_sensitive_contexts_fail_closed_before_preparation() {
    let cases = [
        (
            "computed secret",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ secrets[format('API_{0}', 'TOKEN')] }}\n",
            "secret.context",
        ),
        (
            "matrix-selected secret",
            "on: push\njobs:\n  build:\n    strategy:\n      matrix:\n        secret_name: [API_TOKEN]\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ secrets[matrix.secret_name] }}\n",
            "secret.context",
        ),
        (
            "wildcard secrets",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(secrets.*) }}\n",
            "secret.context",
        ),
        (
            "bare secrets",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(secrets) }}\n",
            "secret.context",
        ),
        (
            "computed GitHub token",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github[format('to{0}', 'ken')] }}\n",
            "credential.github",
        ),
        (
            "matrix-selected GitHub token",
            "on: push\njobs:\n  build:\n    strategy:\n      matrix:\n        token_key: [token]\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github[matrix.token_key] }}\n",
            "credential.github",
        ),
        (
            "wildcard GitHub context",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(github.*) }}\n",
            "credential.github",
        ),
        (
            "bare GitHub context",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(github) }}\n",
            "credential.github",
        ),
    ];

    for (case, workflow, capability) in cases {
        let sandbox = sandbox_with(workflow);
        let output = sandbox.run_with_env(
            &["run", "-W", "wf.yml", "--no-input", "--allow-degraded"],
            &[SSH_DOCKER_HOST],
        );
        assert_eq!(output.status.code(), Some(1), "{case}");
        let stderr = support::stderr_text(&output);
        assert!(
            stderr.contains(&format!("uncertified capability `{capability}`")),
            "{case}: {stderr}"
        );
        assert!(
            !stderr.contains("DOCKER_HOST"),
            "{case} reached engine detection: {stderr}"
        );
        assert!(
            !sandbox.home().join(".litci/daemon").exists(),
            "{case} started the daemon"
        );
        assert_captured_blocked_run(&sandbox);
    }
}

#[test]
fn malformed_secret_arguments_are_redacted_in_every_supported_spelling() {
    const SENTINEL: &str = "malformed_cli_secret_must_not_render_9374";
    let sandbox = sandbox_with(SECRET_WORKFLOW);
    let cases: [&[&str]; 4] = [
        &["run", "-W", "wf.yml", "--secret", SENTINEL],
        &[
            "run",
            "-W",
            "wf.yml",
            "--secret=malformed_cli_secret_must_not_render_9374",
        ],
        &["run", "-W", "wf.yml", "-s", SENTINEL],
        &[
            "run",
            "-W",
            "wf.yml",
            "-smalformed_cli_secret_must_not_render_9374",
        ],
    ];

    for arguments in cases {
        let output = sandbox.run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        let stdout = support::stdout_text(&output);
        let stderr = support::stderr_text(&output);
        assert!(!stdout.contains(SENTINEL), "{arguments:?}: {stdout}");
        assert!(!stderr.contains(SENTINEL), "{arguments:?}: {stderr}");
        assert!(
            stderr.contains("pass each -s/--secret as KEY=VALUE"),
            "{arguments:?}: {stderr}"
        );
    }

    let valid_secret = format!("API_TOKEN={SENTINEL}");
    let long_attached = format!("--secret={valid_secret}");
    let short_attached = format!("-s{valid_secret}");
    let cases: [&[&str]; 4] = [
        &[
            "run",
            "-W",
            "wf.yml",
            "--secret",
            &valid_secret,
            "--unknown",
        ],
        &["run", "-W", "wf.yml", &long_attached, "--unknown"],
        &["run", "-W", "wf.yml", "-s", &valid_secret, "--unknown"],
        &["run", "-W", "wf.yml", &short_attached, "--unknown"],
    ];
    for arguments in cases {
        let output = sandbox.run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        let stdout = support::stdout_text(&output);
        let stderr = support::stderr_text(&output);
        assert!(!stdout.contains(SENTINEL), "{arguments:?}: {stdout}");
        assert!(!stderr.contains(SENTINEL), "{arguments:?}: {stderr}");
        assert!(
            stderr.contains("pass each -s/--secret as KEY=VALUE"),
            "{arguments:?}: {stderr}"
        );
    }

    let typo_value = format!("API_TOKEN={SENTINEL}");
    let typo_cases = [
        ("--secre", format!("--secre={typo_value}")),
        ("--secretx", format!("--secretx={typo_value}")),
    ];
    for (typo, argument) in &typo_cases {
        let output = sandbox.run(&["run", "-W", "wf.yml", argument]);
        assert_eq!(output.status.code(), Some(2), "{argument}");
        let stdout = support::stdout_text(&output);
        let stderr = support::stderr_text(&output);
        assert!(!stdout.contains(SENTINEL), "{argument}: {stdout}");
        assert!(!stderr.contains(SENTINEL), "{argument}: {stderr}");
        assert!(
            stderr.contains("unexpected argument") && stderr.contains(typo),
            "the sanitized typo lost clap's ordinary argument diagnostic: {stderr}"
        );
    }

    const ORDINARY_VALUE: &str = "ordinary_nonsecret_argument_value_2816";
    let ordinary_argument = format!("--unknown={ORDINARY_VALUE}");
    let output = sandbox.run(&["run", "-W", "wf.yml", &ordinary_argument]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("unexpected argument")
            && stderr.contains("--unknown")
            && !stderr.contains("arguments containing a secret value"),
        "ordinary argument diagnostics were weakened: {stderr}"
    );
}

#[test]
fn unused_token_permissions_cross_only_the_explicit_degraded_shell_boundary() {
    let sandbox = sandbox_with(
        "on: push\npermissions:\n  contents: write\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
    );
    let output = sandbox.run_with_env(
        &["run", "-W", "wf.yml", "--allow-degraded"],
        &[SSH_DOCKER_HOST],
    );
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("`--allow-degraded` forced 1 uncertified capability"),
        "{stderr}"
    );
    assert!(
        stderr.contains("DOCKER_HOST"),
        "the explicitly degraded shell run did not reach engine detection: {stderr}"
    );
    assert!(
        !stderr.contains("cannot narrow"),
        "an unused GitHub token triggered credential inspection: {stderr}"
    );
}
