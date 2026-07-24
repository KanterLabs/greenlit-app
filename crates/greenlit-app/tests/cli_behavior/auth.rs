//! Integration oracle for `litci auth` (`PHASE-3-actions.md` Auth exit
//! criterion 5): `--pat` through a piped (non-terminal) token, and the
//! device flow driven end to end against a mocked GitHub endpoint
//! (`support::fake_github::FakeGitHub`) rather than real `github.com`.

use super::support;
use super::support::Sandbox;
use super::support::fake_github::{Canned, FakeGitHub};

const SSH_DOCKER_HOST: (&str, &str) = ("DOCKER_HOST", "ssh://example");

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

#[test]
fn auth_pat_via_piped_stdin_stores_the_token_at_mode_0600() {
    let sandbox = Sandbox::new();
    let output = sandbox.run_with_stdin(&["auth", "--pat"], &[], "ghp_piped_token_value\n");
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(stdout.contains("Variables"), "{stdout}");
    assert!(stdout.contains("Authenticated"), "{stdout}");

    let stored = std::fs::read_to_string(sandbox.home().join(".litci").join("auth.json"))
        .expect("read stored auth file");
    assert!(stored.contains("ghp_piped_token_value"), "{stored}");
    assert!(stored.contains("\"pat\""), "{stored}");

    let metadata = std::fs::metadata(sandbox.home().join(".litci").join("auth.json"))
        .expect("stat stored auth file");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
}

#[test]
fn auth_pat_rejects_empty_piped_input() {
    let sandbox = Sandbox::new();
    let output = sandbox.run_with_stdin(&["auth", "--pat"], &[], "\n");
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("no token was provided"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
}

#[test]
fn a_stored_pat_is_then_used_to_resolve_the_github_token() {
    let sandbox = Sandbox::new();
    let auth_output = sandbox.run_with_stdin(&["auth", "--pat"], &[], "ghp_now_stored\n");
    assert!(
        auth_output.status.success(),
        "{}",
        support::stderr_text(&auth_output)
    );

    sandbox.write("wf.yml", GITHUB_TOKEN_WORKFLOW);
    sandbox.init_git();
    let output = sandbox.run_with_env(&["run", "-W", "wf.yml", "--no-input"], &[SSH_DOCKER_HOST]);
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(
        !stderr.contains("no local token is configured"),
        "the just-stored PAT must be picked up: {stderr}"
    );
    assert!(!stderr.contains("ghp_now_stored"), "{stderr}");
}

#[test]
fn device_flow_succeeds_against_a_mocked_github_endpoint_and_stores_the_token() {
    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    let handle = server.serve(vec![
        Canned::json(
            200,
            "OK",
            r#"{"device_code":"d","user_code":"ABCD-EFGH","verification_uri":"https://github.com/login/device","expires_in":900,"interval":0}"#,
        ),
        Canned::json(
            200,
            "OK",
            r#"{"access_token":"ghu_device_flow_token","token_type":"bearer","scope":"","expires_in":28800,"refresh_token":"ghr_refresh","refresh_token_expires_in":15897600}"#,
        ),
    ]);

    let sandbox = Sandbox::new();
    let output = sandbox.run_with_env(
        &["auth"],
        &[("LITCI_TEST_GITHUB_OAUTH_BASE_URL", base_url.as_str())],
    );
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(stdout.contains("ABCD-EFGH"), "{stdout}");
    assert!(
        stdout.contains("https://github.com/login/device"),
        "{stdout}"
    );
    assert!(stdout.contains("Authenticated"), "{stdout}");

    let stored = std::fs::read_to_string(sandbox.home().join(".litci").join("auth.json"))
        .expect("read stored auth file");
    assert!(stored.contains("ghu_device_flow_token"), "{stored}");
    assert!(stored.contains("\"device_flow\""), "{stored}");
    handle.join().unwrap();
}

#[test]
fn device_flow_reports_denial_with_a_fix() {
    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    let handle = server.serve(vec![
        Canned::json(
            200,
            "OK",
            r#"{"device_code":"d","user_code":"ABCD-EFGH","verification_uri":"https://github.com/login/device","expires_in":900,"interval":0}"#,
        ),
        Canned::json(200, "OK", r#"{"error":"access_denied"}"#),
    ]);

    let sandbox = Sandbox::new();
    let output = sandbox.run_with_env(
        &["auth"],
        &[("LITCI_TEST_GITHUB_OAUTH_BASE_URL", base_url.as_str())],
    );
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("denied"), "{stderr}");
    assert!(stderr.contains("litci auth"), "{stderr}");
    handle.join().unwrap();
}
