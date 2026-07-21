//! Integration tests: the two v0 rejection paths
//! (`PHASE-1-engine-core.md` exit criterion 4) -- a `workflow_call` trigger
//! ("not in v0" with location) and an unsupported `runs-on:` label (the
//! accepted stable-x64 list).

mod support;

use support::Sandbox;

const WORKFLOW_CALL_FIXTURE: &str = include_str!("../../../fixtures/workflow-call.yml");
const UNSUPPORTED_RUNNER_FIXTURE: &str = include_str!("../../../fixtures/unsupported-runner.yml");

#[test]
fn workflow_call_is_rejected_as_not_in_v0_with_location() {
    let sandbox = Sandbox::new();
    let path = sandbox.write("fixtures/workflow-call.yml", WORKFLOW_CALL_FIXTURE);
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "fixtures/workflow-call.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);

    assert!(stderr.contains("workflow_call"));
    assert!(stderr.contains("not in v0"));
    assert!(stderr.contains("fix:"));
    // A real `file:line:col` location, not just the construct name.
    let file_name = path.file_name().unwrap().to_str().unwrap();
    assert!(stderr.contains(file_name), "stderr was: {stderr}");
    assert!(
        stderr.contains(':'),
        "expected a line:col location in {stderr}"
    );
}

#[test]
fn unsupported_runner_label_is_rejected_with_the_accepted_list() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "fixtures/unsupported-runner.yml",
        UNSUPPORTED_RUNNER_FIXTURE,
    );
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "fixtures/unsupported-runner.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);

    assert!(stderr.contains("windows-latest"));
    assert!(stderr.contains("ubuntu-latest"));
    assert!(stderr.contains("ubuntu-24.04"));
    assert!(stderr.contains("ubuntu-22.04"));
    assert!(stderr.contains("fix:"));
}

#[test]
fn stdout_stays_empty_when_planning_fails() {
    let sandbox = Sandbox::new();
    sandbox.write("fixtures/workflow-call.yml", WORKFLOW_CALL_FIXTURE);
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "fixtures/workflow-call.yml", "--json"]);
    assert!(!output.status.success());
    assert!(support::stdout_text(&output).is_empty());
}

#[test]
fn a_failed_plan_still_appends_a_metrics_record() {
    // "every plan/run invocation appends one NDJSON record" (`AGENTS.md`
    // Metrics section) applies even when planning itself fails partway
    // through -- the partial stage timings are still useful, and `litci
    // stats` should see an honest history of every attempt.
    let sandbox = Sandbox::new();
    sandbox.write("fixtures/workflow-call.yml", WORKFLOW_CALL_FIXTURE);
    sandbox.init_git();
    let metrics_file = sandbox.metrics_file();
    assert!(!metrics_file.exists());

    let output = sandbox.run(&["plan", "-W", "fixtures/workflow-call.yml"]);
    assert!(!output.status.success());

    let contents = std::fs::read_to_string(&metrics_file).expect("metrics file must exist");
    let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1);
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["command"], "plan");
}
