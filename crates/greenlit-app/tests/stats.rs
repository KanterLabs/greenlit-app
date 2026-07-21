//! Integration tests: `litci stats` renders local invocation history and
//! per-stage trends and never appends a metrics record itself
//! (`PHASE-1-engine-core.md` exit criterion 6).

pub mod support;

use support::Sandbox;

const WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";

fn metrics_record(version: u32, started_at: u128) -> String {
    format!(
        "{{\"schema_version\":{version},\"command\":\"plan\",\"started_at_unix_ms\":{started_at},\"total_duration_ms\":1.0,\"stages\":[],\"steps\":[],\"hit_miss\":[]}}"
    )
}

fn write_metrics(sandbox: &Sandbox, contents: &str) {
    let path = sandbox.metrics_file();
    std::fs::create_dir_all(path.parent().expect("metrics parent")).expect("create metrics dir");
    std::fs::write(path, contents).expect("write metrics history");
}

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
    assert!(stdout.contains("recent invocations (up to 20, 2 shown)"));
    assert!(stdout.contains("stage trends"));
    assert!(stdout.contains("parse"));
    assert!(stdout.contains("eval"));
    assert!(stdout.contains("plan"));

    // Read-only: `stats` must never append a metrics record for itself.
    let after = std::fs::read_to_string(sandbox.metrics_file()).expect("metrics file exists");
    let lines_after = after.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(lines_after, lines_before);
}

#[test]
fn stats_bounds_history_and_the_next_append_repairs_an_unterminated_tail() {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", WORKFLOW);
    sandbox.init_git();
    let mut history = (0..25)
        .map(|index| metrics_record(1, index))
        .collect::<Vec<_>>()
        .join("\n");
    history.push_str("\n{\"schema_version\":1");
    write_metrics(&sandbox, &history);

    let output = sandbox.run(&["stats"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(stdout.contains("up to 20, 20 shown"));
    assert!(!stdout.contains("t=4 "));
    assert!(stdout.contains("t=24 "));

    let plan_output = sandbox.run(&["plan", "-W", "wf.yml"]);
    assert!(
        plan_output.status.success(),
        "{}",
        support::stderr_text(&plan_output)
    );
    let repaired = std::fs::read_to_string(sandbox.metrics_file()).expect("metrics file exists");
    let lines = repaired.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 26);
    assert!(
        lines
            .iter()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    let stats_output = sandbox.run(&["stats"]);
    assert!(
        stats_output.status.success(),
        "{}",
        support::stderr_text(&stats_output)
    );

    let complete_tail = Sandbox::new();
    complete_tail.write("wf.yml", WORKFLOW);
    complete_tail.init_git();
    write_metrics(&complete_tail, &metrics_record(1, 7));
    let plan_output = complete_tail.run(&["plan", "-W", "wf.yml"]);
    assert!(
        plan_output.status.success(),
        "{}",
        support::stderr_text(&plan_output)
    );
    let preserved = std::fs::read_to_string(complete_tail.metrics_file())
        .expect("metrics history must remain readable");
    assert_eq!(preserved.lines().count(), 2);
    assert!(preserved.starts_with(&format!("{}\n", metrics_record(1, 7))));
    let stats_output = complete_tail.run(&["stats"]);
    assert!(
        stats_output.status.success(),
        "{}",
        support::stderr_text(&stats_output)
    );
    assert!(support::stdout_text(&stats_output).contains("t=7 "));
}

#[test]
fn stats_rejects_committed_corruption_and_unknown_schema_with_one_fix() {
    for (contents, expected, fix) in [
        ("not-json\n".to_string(), "corrupt metrics record", "move"),
        (
            format!("{}\n", metrics_record(99, 1)),
            "unsupported metrics schema version 99",
            "update litci",
        ),
    ] {
        let sandbox = Sandbox::new();
        write_metrics(&sandbox, &contents);
        let output = sandbox.run(&["stats"]);
        assert!(!output.status.success());
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(stderr.contains("fix:"), "{stderr}");
        assert!(stderr.contains(fix), "{stderr}");
    }
}
