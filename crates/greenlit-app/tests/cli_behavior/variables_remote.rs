//! Integration oracle for the authenticated GitHub configuration-variable
//! lookup (`PHASE-3-actions.md` Variables exit criterion 2): repository-
//! over-organization precedence, an authenticated absent name resolving to
//! the empty string, a dynamic `vars[...]` complete-map fetch, and an API
//! permission error naming the exact fix. Every case drives a mocked
//! external GitHub endpoint (`support::fake_github::FakeGitHub`), never real
//! `api.github.com` (exit criterion 5).

use super::common::LITERAL_VAR_WORKFLOW;
use super::support;
use super::support::Sandbox;
use super::support::fake_github::{Canned, FakeGitHub};

const DYNAMIC_LOOKUP_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    if: vars[github.event_name] == 'yes'
    steps:
      - run: echo hi
";

const ABSENT_VAR_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    if: vars.MODE == ''
    steps:
      - run: echo hi
";

fn authenticated_sandbox(workflow: &str) -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", workflow);
    sandbox.init_git();
    sandbox.seed_auth_token("ghu_test_token");
    sandbox
}

fn condition_line(stdout: &str) -> &str {
    stdout
        .lines()
        .find(|l| l.trim_start().starts_with("if:"))
        .unwrap_or_else(|| panic!("no 'if:' line in stdout: {stdout}"))
}

#[test]
fn repository_variable_overrides_organization_variable_of_the_same_name() {
    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    // `crate::vars::fetch_complete_map` fetches organization variables
    // first, then repository variables, merging repository values over
    // organization ones.
    let handle = server.serve(vec![
        Canned::json(
            200,
            "OK",
            r#"{"total_count":1,"variables":[{"name":"push","value":"no"}]}"#,
        ),
        Canned::json(
            200,
            "OK",
            r#"{"total_count":1,"variables":[{"name":"push","value":"yes"}]}"#,
        ),
    ]);

    let sandbox = authenticated_sandbox(DYNAMIC_LOOKUP_WORKFLOW);
    let output = sandbox.run_with_env(
        &["plan", "-W", "wf.yml"],
        &[("LITCI_TEST_GITHUB_API_BASE_URL", base_url.as_str())],
    );
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    assert!(
        condition_line(&support::stdout_text(&output)).contains("static(true)"),
        "repository value 'yes' must win over organization value 'no': {}",
        support::stdout_text(&output)
    );
    handle.join().unwrap();
}

#[test]
fn an_authenticated_absent_name_resolves_to_the_empty_string() {
    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    // `crate::vars::fetch_named`: repository lookup first, then
    // organization fallback; both report the variable absent.
    let handle = server.serve(vec![
        Canned::json(404, "Not Found", r#"{"message":"Not Found"}"#),
        Canned::json(404, "Not Found", r#"{"message":"Not Found"}"#),
    ]);

    let sandbox = authenticated_sandbox(ABSENT_VAR_WORKFLOW);
    let output = sandbox.run_with_env(
        &["plan", "-W", "wf.yml"],
        &[("LITCI_TEST_GITHUB_API_BASE_URL", base_url.as_str())],
    );
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    assert!(
        condition_line(&support::stdout_text(&output)).contains("static(true)"),
        "an absent GitHub variable must resolve to the empty string, matching GitHub: {}",
        support::stdout_text(&output)
    );
    handle.join().unwrap();
}

#[test]
fn an_api_permission_error_names_the_needed_permission_and_never_falls_back() {
    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    // A 403 on the very first (repository) lookup must stop immediately —
    // it must not be treated as "absent" and fall through to the
    // organization endpoint.
    let handle = server.serve(vec![Canned::json(
        403,
        "Forbidden",
        r#"{"message":"Resource not accessible by integration"}"#,
    )]);

    let sandbox = authenticated_sandbox(LITERAL_VAR_WORKFLOW);
    let output = sandbox.run_with_env(
        &["plan", "-W", "wf.yml"],
        &[("LITCI_TEST_GITHUB_API_BASE_URL", base_url.as_str())],
    );
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("Variables"), "{stderr}");
    assert!(stderr.contains("repository"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
    handle.join().unwrap();
}

#[test]
fn a_lookup_only_run_never_mentions_or_needs_a_github_token() {
    // `vars.MODE` is supplied locally, so no remote lookup happens at all —
    // but the point of this test is the *token*, not the variable: a
    // workflow that never references `secrets.GITHUB_TOKEN`/`github.token`
    // must never see, print, or otherwise act on the stored token, even
    // though one is configured and would be available if anything asked for
    // it (`PHASE-3-actions.md` exit criterion 2: "no workflow token
    // injection for lookup-only runs").
    const SENTINEL: &str = "must-never-appear-in-any-output-7391";
    let sandbox = authenticated_sandbox(LITERAL_VAR_WORKFLOW);
    sandbox.seed_auth_token(SENTINEL);

    let output = sandbox.run_with_env(
        &["run", "-W", "wf.yml", "--var", "MODE=ci"],
        &[("DOCKER_HOST", "ssh://example")],
    );
    // Engine detection must still be reached (proves resolution completed
    // normally) and must fail for the expected, Docker-agnostic reason.
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("DOCKER_HOST"), "{stderr}");
    assert!(!stderr.contains(SENTINEL), "{stderr}");
    assert!(!stderr.contains("GITHUB_TOKEN"), "{stderr}");
    assert!(!stderr.contains("github.token"), "{stderr}");
    assert!(!support::stdout_text(&output).contains(SENTINEL));
}
