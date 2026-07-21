//! Integration tests: `litci stats` renders local invocation history and
//! per-stage trends and never appends a metrics record itself
//! (`PHASE-1-engine-core.md` exit criterion 6).

mod support;

use support::Sandbox;

const WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";

#[test]
fn stats_on_empty_history_reports_no_history_without_creating_the_file() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["stats"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(stdout.contains("no invocation history yet"));
    assert!(!sandbox.metrics_file().exists());
}

#[test]
fn stats_renders_recorded_invocations_and_stage_trends_without_appending() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", WORKFLOW);
    sandbox.init_git();

    for _ in 0..2 {
        let plan_output = sandbox.run(&["plan", "-W", "wf.yml"]);
        assert!(
            plan_output.status.success(),
            "{}",
            support::stderr_text(&plan_output)
        );
    }

    let before = std::fs::read_to_string(sandbox.metrics_file()).expect("metrics file exists");
    let lines_before = before.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(lines_before, 2);

    let output = sandbox.run(&["stats"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(stdout.contains("recent invocations (2 total)"));
    assert!(stdout.contains("stage trends"));
    assert!(stdout.contains("parse"));
    assert!(stdout.contains("eval"));
    assert!(stdout.contains("plan"));

    // Read-only: `stats` must never append a metrics record for itself.
    let after = std::fs::read_to_string(sandbox.metrics_file()).expect("metrics file exists");
    let lines_after = after.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(lines_after, lines_before);
}
