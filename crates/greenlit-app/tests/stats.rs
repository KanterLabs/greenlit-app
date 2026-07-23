//! Integration tests: `litci stats` renders local invocation history and
//! per-stage trends and never appends a metrics record itself
//! (`PHASE-1-engine-core.md` exit criterion 6).

pub mod support;

#[path = "stats/bounded_history.rs"]
mod bounded_history;

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

fn write_metrics(sandbox: &Sandbox, contents: impl AsRef<[u8]>) {
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

    // Empty/relative HOME values must not redirect user-local metrics into
    // the repository that happens to be the invocation cwd.
    for home in ["", "relative-home"] {
        let output = sandbox.run_with_env(&["stats"], &[("HOME", home)]);
        assert!(!output.status.success());
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains("HOME is empty, relative"), "{stderr}");
        assert!(stderr.contains("absolute user home directory"), "{stderr}");
    }
    assert!(!sandbox.root().join(".litci").exists());
    assert!(!sandbox.root().join("relative-home").exists());

    // HOME itself may legitimately be a symlink; Greenlit resolves that one
    // boundary before enforcing no-follow semantics below it.
    let real_home = tempfile::tempdir().expect("real HOME");
    let home_link_parent = tempfile::tempdir().expect("HOME link parent");
    let home_link = home_link_parent.path().join("home-link");
    std::os::unix::fs::symlink(real_home.path(), &home_link).expect("link HOME");
    let output = sandbox.run_with_env(
        &["stats"],
        &[(
            "HOME",
            home_link.to_str().expect("test HOME path must be UTF-8"),
        )],
    );
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    assert!(support::stdout_text(&output).contains("no invocation history yet"));
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
    write_metrics(&complete_tail, metrics_record(1, 7));
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

    // Tail repair validates JSON completeness independently from schema
    // compatibility. A future record whose shape this build cannot decode is
    // preserved, separated from the next append, and then reported by stats
    // with the one compatible-version action.
    let future_tail = Sandbox::new();
    future_tail.write("wf.yml", WORKFLOW);
    future_tail.init_git();
    write_metrics(
        &future_tail,
        r#"{"schema_version":99,"future_shape":{"required":true}}"#,
    );
    let plan_output = future_tail.run(&["plan", "-W", "wf.yml"]);
    assert!(
        plan_output.status.success(),
        "{}",
        support::stderr_text(&plan_output)
    );
    let preserved = std::fs::read_to_string(future_tail.metrics_file())
        .expect("future-schema history must remain readable");
    assert_eq!(preserved.lines().count(), 2);
    assert!(preserved.starts_with("{\"schema_version\":99,"));
    let stats_output = future_tail.run(&["stats"]);
    assert!(!stats_output.status.success());
    let stderr = support::stderr_text(&stats_output);
    assert!(
        stderr.contains("unsupported metrics schema version 99"),
        "{stderr}"
    );
    assert!(stderr.contains("update litci"), "{stderr}");

    // An oversized suffix is still uncommitted because it has no newline.
    // The next append discards it at the last committed boundary without
    // trying to materialize the suffix as one in-memory record.
    const MAX_METRICS_RECORD_BYTES: usize = 8 * 1024 * 1024;
    let oversized_tail = Sandbox::new();
    oversized_tail.write("wf.yml", WORKFLOW);
    oversized_tail.init_git();
    let mut history = format!("{}\n", metrics_record(1, 9));
    history.push_str(&"x".repeat(MAX_METRICS_RECORD_BYTES + 1));
    write_metrics(&oversized_tail, history);
    let plan_output = oversized_tail.run(&["plan", "-W", "wf.yml"]);
    assert!(
        plan_output.status.success(),
        "{}",
        support::stderr_text(&plan_output)
    );
    let repaired = std::fs::read_to_string(oversized_tail.metrics_file())
        .expect("oversized torn tail must be repaired");
    assert_eq!(repaired.lines().count(), 2);
    assert!(
        repaired
            .lines()
            .all(|line| { serde_json::from_str::<serde_json::Value>(line).is_ok() })
    );
}

#[test]
fn stats_rejects_committed_corruption_and_unknown_schema_with_one_fix() {
    const MAX_METRICS_RECORD_BYTES: usize = 8 * 1024 * 1024;
    let mut oversized_record = vec![b'x'; MAX_METRICS_RECORD_BYTES + 1];
    oversized_record.push(b'\n');
    for (contents, expected, fix) in [
        (b"not-json\n".to_vec(), "corrupt metrics record", "move"),
        (vec![0xff, b'\n'], "corrupt metrics record", "move"),
        (oversized_record, "per-record safety limit", "move"),
        (
            b"{\"schema_version\":99}\n".to_vec(),
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

    // `stats` must not follow the final metrics-file component into an
    // arbitrary local target.
    let linked = Sandbox::new();
    let external = tempfile::tempdir().expect("external metrics directory");
    let target = external.path().join("redirected.ndjson");
    std::fs::write(&target, format!("{}\n", metrics_record(1, 77)))
        .expect("write redirected history");
    let path = linked.metrics_file();
    std::fs::create_dir_all(path.parent().expect("metrics parent")).expect("create metrics parent");
    std::os::unix::fs::symlink(&target, &path).expect("link metrics file");
    let output = linked.run(&["stats"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("symbolic link or special file"), "{stderr}");
    assert!(!support::stdout_text(&output).contains("t=77 "));

    // A FIFO at the metrics path must be identified without waiting for a
    // producer. The short-lived nonblocking writer makes the old blocking
    // read path complete (and therefore fail this assertion) rather than
    // allowing a regression to hang the test process forever.
    let fifo = Sandbox::new();
    let path = fifo.metrics_file();
    std::fs::create_dir_all(path.parent().expect("metrics parent")).expect("create metrics parent");
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("spawn mkfifo");
    assert!(status.success(), "mkfifo failed");
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        for _ in 0..100 {
            match rustix::fs::open(
                &writer_path,
                rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::empty(),
            ) {
                Ok(_) => return,
                Err(rustix::io::Errno::NXIO) => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
    });
    let output = fifo.run(&["stats"]);
    writer.join().expect("FIFO writer must not panic");
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("symbolic link or special file"), "{stderr}");
}

#[test]
fn concurrent_plans_preserve_every_record_while_repairing_one_torn_tail() {
    const CONCURRENT_PLANS: usize = 12;
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", WORKFLOW);
    sandbox.init_git();

    let mut history = format!("{}\n", metrics_record(1, 42));
    history.push_str(&"x".repeat(4 * 1024 * 1024));
    write_metrics(&sandbox, history);

    let outputs = std::thread::scope(|scope| {
        let handles = (0..CONCURRENT_PLANS)
            .map(|_| scope.spawn(|| sandbox.run(&["plan", "-W", "wf.yml"])))
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("plan thread must not panic"))
            .collect::<Vec<_>>()
    });
    for output in outputs {
        assert!(output.status.success(), "{}", support::stderr_text(&output));
    }

    let contents = std::fs::read_to_string(sandbox.metrics_file())
        .expect("concurrent metrics history must exist");
    let lines = contents.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), CONCURRENT_PLANS + 1);
    assert!(
        lines
            .iter()
            .all(|line| { serde_json::from_str::<serde_json::Value>(line).is_ok() })
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("\"started_at_unix_ms\":42"))
            .count(),
        1
    );

    let stats = sandbox.run(&["stats"]);
    assert!(stats.status.success(), "{}", support::stderr_text(&stats));
}

#[test]
fn stats_keeps_tampered_local_record_fields_on_their_renderer_lines() {
    let sandbox = Sandbox::new();
    let record = serde_json::json!({
        "schema_version": 1,
        "command": "plan\nFORGED COMMAND\t\u{202e}",
        "started_at_unix_ms": 7,
        "total_duration_ms": 1.0,
        "stages": [{
            "name": "parse\nFORGED STAGE\u{1b}[2J",
            "duration_ms": 1.0
        }],
        "steps": [],
        "hit_miss": []
    });
    write_metrics(&sandbox, format!("{record}\n"));

    let output = sandbox.run(&["stats"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let stdout = support::stdout_text(&output);
    assert!(
        stdout.contains(r"plan\nFORGED COMMAND\t\u{202e}"),
        "{stdout}"
    );
    assert!(stdout.contains(r"parse\nFORGED STAGE\u{1b}[2J"), "{stdout}");
    assert!(!stdout.contains("\nFORGED COMMAND"));
    assert!(!stdout.contains("\nFORGED STAGE"));
    assert!(!stdout.contains('\u{202e}'));
    assert!(!stdout.contains('\u{1b}'));
}
