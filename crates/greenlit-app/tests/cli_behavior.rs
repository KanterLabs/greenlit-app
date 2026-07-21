//! Integration tests for CLI-level plumbing that is not specific to the
//! `matrix-needs` fixture's structure: local `vars.*` resolution precedence
//! (CLI > process environment > `.litci/vars`), missing-variable guidance,
//! dynamic `vars[...]` handling, and workflow discovery when `-W` is
//! omitted (`PHASE-1-engine-core.md` greenlit-app section, exit criterion
//! 5). Each test writes its own small, throwaway workflow into the sandbox
//! rather than growing `fixtures/matrix-needs.yml` with unrelated scenarios
//! (`TESTING.md`: "No duplicate homes").

mod support;

use support::Sandbox;

const LITERAL_VAR_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    if: vars.MODE == 'ci'
    steps:
      - run: echo hi
";

const DYNAMIC_VAR_WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    if: vars[github.event_name] == 'yes'
    steps:
      - run: echo hi
";

fn condition_line(stdout: &str) -> &str {
    stdout
        .lines()
        .find(|l| l.trim_start().starts_with("if:"))
        .unwrap_or_else(|| panic!("no 'if:' line in stdout: {stdout}"))
}

#[test]
fn missing_literal_var_fails_with_location_and_fix() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", LITERAL_VAR_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("vars.MODE"));
    assert!(stderr.contains("is not set"));
    assert!(stderr.contains("wf.yml:"));
    assert!(stderr.contains("fix:"));
    assert!(stderr.contains("--var"));
    assert!(stderr.contains(".litci/vars"));

    // A failed plan still appends a metrics record (`AGENTS.md` Metrics
    // section: "every plan/run invocation appends one NDJSON record").
    let contents = std::fs::read_to_string(sandbox.metrics_file()).expect("metrics file exists");
    let record: serde_json::Value =
        serde_json::from_str(contents.trim_end()).expect("one NDJSON record");
    assert_eq!(record["command"], "plan");
}

#[test]
fn cli_var_resolves_the_condition() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", LITERAL_VAR_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "wf.yml", "--var", "MODE=ci"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(condition_line(&stdout).contains("static(true)"));
}

#[test]
fn process_env_var_resolves_the_condition_when_no_cli_override_is_given() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", LITERAL_VAR_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run_with_env(&["plan", "-W", "wf.yml"], &[("MODE", "ci")]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(condition_line(&stdout).contains("static(true)"));
}

#[test]
fn dotenv_file_resolves_the_condition_when_nothing_else_is_given() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", LITERAL_VAR_WORKFLOW);
    sandbox.write(".litci/vars", "MODE=ci\n");
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(condition_line(&stdout).contains("static(true)"));
}

#[test]
fn cli_override_wins_over_dotenv() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", LITERAL_VAR_WORKFLOW);
    sandbox.write(".litci/vars", "MODE=ci\n");
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "wf.yml", "--var", "MODE=not-ci"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(condition_line(&stdout).contains("static(false)"));
}

#[test]
fn process_env_wins_over_dotenv() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", LITERAL_VAR_WORKFLOW);
    sandbox.write(".litci/vars", "MODE=ci\n");
    sandbox.init_git();

    let output = sandbox.run_with_env(&["plan", "-W", "wf.yml"], &[("MODE", "not-ci")]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(condition_line(&stdout).contains("static(false)"));
}

#[test]
fn dynamic_vars_lookup_plans_successfully_without_any_local_override() {
    // A dynamic `vars[...]` lookup can never be validated ahead of time (the
    // referenced name is not known statically), so -- unlike a literal
    // `vars.NAME` reference -- its absence from every local source is never
    // a hard planning error; it simply resolves to an empty string per the
    // evaluator's documented unknown-key rule.
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", DYNAMIC_VAR_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(condition_line(&stdout).contains("static(false)"));
}

#[test]
fn dynamic_vars_lookup_resolves_when_the_supplied_map_covers_it() {
    // The synthetic push event's `github.event_name` is `"push"`, so
    // supplying `--var push=yes` (even though the workflow never spells
    // `vars.push` literally) is what "a complete locally supplied map"
    // means in this no-auth phase.
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", DYNAMIC_VAR_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "wf.yml", "--var", "push=yes"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(condition_line(&stdout).contains("static(true)"));
}

#[test]
fn workflow_is_discovered_when_exactly_one_exists_under_github_workflows() {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/ci.yml", LITERAL_VAR_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run(&["plan", "--var", "MODE=ci"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    assert!(support::stdout_text(&output).contains("event: push"));
}

#[test]
fn missing_workflows_directory_fails_with_a_fix_naming_dash_w() {
    let sandbox = Sandbox::new();
    sandbox.init_git();

    let output = sandbox.run(&["plan"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("-W"));
    assert!(stderr.contains(".github/workflows"));
}

#[test]
fn ambiguous_workflows_directory_fails_and_lists_the_candidates() {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/a.yml", LITERAL_VAR_WORKFLOW);
    sandbox.write(".github/workflows/b.yml", LITERAL_VAR_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run(&["plan"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("a.yml"));
    assert!(stderr.contains("b.yml"));
    assert!(stderr.contains("-W"));
}
