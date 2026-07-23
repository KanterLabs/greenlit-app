//! Integration tests: `litci plan` against `fixtures/matrix-needs.yml`
//! (`TESTING.md` integration-test class; `PHASE-1-engine-core.md` exit
//! criteria 2 and 5). Asserts the *resolved plan structure* -- job count and
//! order, matrix expansion, output/needs wiring, static vs. deferred `if:`
//! markers -- plus local-variable and missing-property semantics through
//! variants derived from that same fixture, not just a zero exit code.

pub mod support;

use std::os::unix::fs::symlink;
use std::process::Output;

use support::Sandbox;

/// Embedded at compile time so the checked-in fixture is exactly what every
/// test below plans against, with no runtime dependency on the test
/// runner's own working directory.
const MATRIX_NEEDS_FIXTURE: &str = include_str!("../../../fixtures/matrix-needs.yml");
const MATRIX_NEEDS_PLAN_GOLDEN: &str = include_str!("../../../fixtures/matrix-needs.plan.json");
const MATRIX_NEEDS_PATH: &str = "fixtures/matrix-needs.yml";
const CANONICAL_DEPLOY_CONDITION: &str = "    if: github.event_name == 'push'";

/// Writes the fixture into a fresh sandbox and returns it, ready to `plan`.
fn sandbox_with_fixture() -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write(MATRIX_NEEDS_PATH, MATRIX_NEEDS_FIXTURE);
    sandbox.init_git();
    sandbox
}

/// Derives a variable/unknown-property oracle case from the named fixture
/// while leaving its checked-in, environment-independent golden unchanged.
fn sandbox_with_deploy_condition(condition: &str) -> Sandbox {
    assert_eq!(
        MATRIX_NEEDS_FIXTURE
            .matches(CANONICAL_DEPLOY_CONDITION)
            .count(),
        1,
        "matrix-needs must have one canonical deploy-condition anchor"
    );
    let fixture = MATRIX_NEEDS_FIXTURE.replacen(
        CANONICAL_DEPLOY_CONDITION,
        &format!("    if: {condition}"),
        1,
    );
    let sandbox = Sandbox::new();
    sandbox.write(MATRIX_NEEDS_PATH, &fixture);
    sandbox.init_git();
    sandbox
}

fn deploy_condition_value(output: &Output) -> bool {
    assert!(
        output.status.success(),
        "matrix-needs variant failed: {}",
        support::stderr_text(output)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("variant plan stdout must be valid JSON");
    let deploy = plan["jobs"]
        .as_array()
        .expect("jobs array")
        .iter()
        .find(|job| job["id"] == "deploy")
        .expect("deploy job");
    assert_eq!(deploy["condition"]["evaluation"], "static");
    deploy["condition"]["value"]
        .as_bool()
        .expect("static deploy condition must be boolean")
}

#[path = "cli_behavior/variables.rs"]
mod variables;

#[test]
fn golden_plan_owns_the_matrix_needs_structure_and_is_byte_stable() {
    let sandbox = sandbox_with_fixture();
    let first = sandbox.run(&["plan", "-W", "fixtures/matrix-needs.yml", "--json"]);
    let second = sandbox.run(&["plan", "-W", "fixtures/matrix-needs.yml", "--json"]);
    assert!(
        first.status.success(),
        "first plan failed: {}",
        support::stderr_text(&first)
    );
    assert!(
        second.status.success(),
        "second plan failed: {}",
        support::stderr_text(&second)
    );
    assert_eq!(first.stdout, MATRIX_NEEDS_PLAN_GOLDEN.as_bytes());
    assert_eq!(first.stdout, second.stdout);

    let plan: serde_json::Value =
        serde_json::from_slice(&first.stdout).expect("plan --json stdout must be valid JSON");
    let jobs = plan["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 3);
    assert_eq!(
        jobs.iter()
            .map(|job| job["id"].as_str().expect("job id"))
            .collect::<Vec<_>>(),
        ["build", "test", "deploy"]
    );
    assert_eq!(
        plan["topo_order"],
        serde_json::json!(["build", "test", "deploy"])
    );
    assert_eq!(jobs[0]["needs"], serde_json::json!([]));
    assert_eq!(jobs[0]["wave"], 0);
    assert_eq!(jobs[1]["needs"], serde_json::json!(["build"]));
    assert_eq!(jobs[1]["wave"], 1);
    assert_eq!(jobs[2]["needs"], serde_json::json!(["build", "test"]));
    assert_eq!(jobs[2]["wave"], 2);

    // `exclude: [{channel: beta}]` drops both beta combinations; `include`
    // adds `canary` to the surviving 24.04 leg instead of creating a third.
    let build = &jobs[0];
    let legs = build["strategy"]["matrix"]["legs"]
        .as_array()
        .expect("matrix legs");
    assert_eq!(legs.len(), 2);
    assert_eq!(legs[0]["values"]["os"]["value"], "ubuntu-22.04");
    assert_eq!(legs[0]["values"]["channel"]["value"], "stable");
    assert!(legs[0]["values"].get("canary").is_none());
    assert_eq!(legs[1]["values"]["os"]["value"], "ubuntu-24.04");
    assert_eq!(legs[1]["values"]["channel"]["value"], "stable");
    assert_eq!(legs[1]["values"]["canary"]["value"], true);
    assert_eq!(build["legs"][0]["runner"]["value"], "ubuntu-22.04");
    assert_eq!(build["legs"][1]["runner"]["value"], "ubuntu-24.04");
    assert_eq!(
        build["legs"][0]["name"]["value"],
        "build (ubuntu-22.04, stable)"
    );
    assert_eq!(
        build["legs"][1]["name"]["value"],
        "build (ubuntu-24.04, stable, true)"
    );
    assert!(build["runner"].is_null());
    assert!(build["condition"].is_null());
    assert!(build["outputs"]["entries"].as_object().unwrap().is_empty());
    assert!(build["steps"].as_array().unwrap().is_empty());

    // Matrix outputs remain deferred on each leg and are consumed by the
    // downstream deploy output. The corresponding collision lint is kept.
    for leg in build["legs"].as_array().expect("build legs") {
        let output = &leg["outputs"]["entries"]["artifact-name"];
        assert_eq!(output["evaluation"], "deferred");
        assert_eq!(output["residual"], "steps.pack.outputs.name");
    }
    let deploy_output = &jobs[2]["outputs"]["entries"]["release-tag"];
    assert_eq!(deploy_output["evaluation"], "deferred");
    assert!(
        deploy_output["defers_on"]
            .as_array()
            .expect("deploy deferrals")
            .iter()
            .any(|reason| reason["kind"] == "needs-output"
                && reason["job"] == "build"
                && reason["output"] == "artifact-name")
    );
    assert!(
        plan["lints"]
            .as_array()
            .expect("lints")
            .iter()
            .any(|lint| lint["kind"] == "matrix-outputs-collision")
    );
    assert!(
        !plan["lints"]
            .as_array()
            .expect("lints")
            .iter()
            .any(|lint| lint["kind"] == "dead-exclude")
    );

    assert_eq!(jobs[2]["condition"]["evaluation"], "static");
    assert_eq!(jobs[2]["condition"]["value"], true);
    let upload = jobs[1]["steps"]
        .as_array()
        .expect("test steps")
        .iter()
        .find(|step| step["name"]["value"] == "Upload coverage")
        .expect("Upload coverage step");
    assert_eq!(upload["condition"]["evaluation"], "deferred");
    assert!(
        upload["condition"]["defers_on"]
            .as_array()
            .expect("condition deferrals")
            .iter()
            .any(|reason| reason["kind"] == "needs-result" && reason["job"] == "build")
    );

    // Timings/run metadata never leak into stdout.
    let stdout = support::stdout_text(&first);
    assert!(!stdout.contains("stage timings"));
    assert!(!stdout.contains("started_at_unix_ms"));
}

#[test]
fn human_tree_output_and_stderr_diagnostics_are_separated_and_terminal_safe() {
    let sandbox = sandbox_with_fixture();
    let output = sandbox.run(&["plan", "-W", "fixtures/matrix-needs.yml"]);
    assert!(output.status.success());
    let stdout = support::stdout_text(&output);
    let stderr = support::stderr_text(&output);

    assert!(stdout.contains("event: push"));
    assert!(stdout.contains("topo order: build -> test -> deploy"));
    assert!(stdout.contains("build"));
    assert!(stdout.contains("deploy"));
    // Diagnostics and timings never leak into stdout.
    assert!(!stdout.contains("stage timings"));
    assert!(!stdout.contains("warning:"));

    // The matrix-outputs-collision lint and stage timings land on stderr.
    assert!(stderr.contains("warning:"));
    assert!(stderr.contains("stage timings (plan):"));

    // A cloned repository is untrusted input. YAML escape syntax can encode
    // CSI/OSC terminal commands in names and scripts; human output must show
    // them visibly instead of sending raw controls to the user's terminal.
    let hostile = Sandbox::new();
    hostile.write(
        "hostile.yml",
        r#"
name: hostile
on: push
jobs:
  hostile:
    name: "\e[2J\e[HFAKE PLAN\nINJECTED JOB\t\u202eRTL\u2066LRI"
    runs-on: ubuntu-latest
    steps:
      - name: "\e]8;;https://attacker.invalid\aClick me\e]8;;\a\nINJECTED STEP\t\u202e"
        run: "printf '\e]52;c;UHdOZWQ=\a'"
"#,
    );
    hostile.init_git();
    let hostile_output = hostile.run(&["plan", "-W", "hostile.yml"]);
    assert!(hostile_output.status.success());
    let hostile_stdout = support::stdout_text(&hostile_output);
    let hostile_stderr = support::stderr_text(&hostile_output);
    for output in [&hostile_stdout, &hostile_stderr] {
        assert!(
            !output
                .chars()
                .any(|character| character.is_control() && character != '\n' && character != '\t'),
            "terminal control reached human output: {output:?}"
        );
    }
    assert!(hostile_stdout.contains(r"\u{1b}[2J\u{1b}[HFAKE PLAN"));
    assert!(hostile_stdout.contains(r"\u{1b}]8;;https://attacker.invalid\u{7}"));
    assert!(hostile_stdout.contains(r"\nINJECTED JOB\t\u{202e}RTL\u{2066}LRI"));
    assert!(hostile_stdout.contains(r"\nINJECTED STEP\t\u{202e}"));
    assert!(!hostile_stdout.contains("\nINJECTED JOB"));
    assert!(!hostile_stdout.contains("\nINJECTED STEP"));
    assert!(!hostile_stdout.contains('\u{202e}'));
    assert!(!hostile_stdout.contains('\u{2066}'));

    // The final error boundary applies the same rule to span-bearing
    // diagnostics, including a hostile tracked workflow filename.
    let hostile_path = "\u{1b}]52;c;UHdOZWQ=\u{7}\nINJECTED ERROR\t\u{202e}.yml";
    let failing = Sandbox::new();
    failing.write(
        hostile_path,
        "on: workflow_call\njobs:\n  call:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
    );
    failing.init_git();
    let error_output = failing.run(&["plan", "-W", hostile_path]);
    assert!(!error_output.status.success());
    let error_stderr = support::stderr_text(&error_output);
    assert!(!error_stderr.contains('\u{1b}'));
    assert!(error_stderr.contains(r"\u{1b}]52;c;UHdOZWQ=\u{7}\nINJECTED ERROR\t\u{202e}.yml"));
    assert!(!error_stderr.contains("\nINJECTED ERROR"));
    assert!(!error_stderr.contains('\u{202e}'));

    // Static vars extraction can surface an invalid bracket key in a
    // multi-line diagnostic. The key and span remain one escaped inline
    // value; authored text cannot forge a second `fix:` line.
    let hostile_var = Sandbox::new();
    hostile_var.write(
        "vars.yml",
        r#"on: push
jobs:
  build:
    runs-on: ubuntu-latest
    if: "${{ vars['BAD\n  fix: FORGED VAR\t\u202e'] == 'x' }}"
    steps:
      - run: echo hi
"#,
    );
    hostile_var.init_git();
    let var_output = hostile_var.run(&["plan", "-W", "vars.yml"]);
    assert!(!var_output.status.success());
    let var_stderr = support::stderr_text(&var_output);
    assert!(
        var_stderr.contains(r"BAD\n  fix: FORGED VAR\t\u{202e}"),
        "{var_stderr}"
    );
    assert!(!var_stderr.contains("\n  fix: FORGED VAR"));
    assert!(!var_stderr.contains('\u{202e}'));

    // Discovery failures render a hostile explicit path without allowing
    // its LF/tab/bidi characters to create a forged diagnostic line.
    let hostile_missing_path = "missing\nFORGED DISCOVERY\t\u{202e}.yml";
    let discovery = Sandbox::new();
    discovery.init_git();
    let discovery_output = discovery.run(&["plan", "-W", hostile_missing_path]);
    assert!(!discovery_output.status.success());
    let discovery_stderr = support::stderr_text(&discovery_output);
    assert!(
        discovery_stderr.contains(r"missing\nFORGED DISCOVERY\t\u{202e}.yml"),
        "{discovery_stderr}"
    );
    assert!(!discovery_stderr.contains("\nFORGED DISCOVERY"));
    assert!(!discovery_stderr.contains('\u{202e}'));

    // clap's own invalid-value path is also an output boundary; it must not
    // bypass the same terminal-control protection before command dispatch.
    let invalid_event = "bad\nFORGED CLAP\t\u{202e}\u{1b}[2J";
    let clap_output = discovery.run(&["plan", "-e", invalid_event]);
    assert!(!clap_output.status.success());
    let clap_stderr = support::stderr_text(&clap_output);
    assert!(
        clap_stderr.contains(r"bad\nFORGED CLAP\t\u{202e}\u{1b}[2J"),
        "{clap_stderr}"
    );
    assert!(!clap_stderr.contains("\nFORGED CLAP"));
    assert!(!clap_stderr.contains('\u{202e}'));
    assert!(!clap_stderr.contains('\u{1b}'));
}

#[test]
fn plan_appends_exactly_one_metrics_record() {
    let sandbox = sandbox_with_fixture();
    let metrics_file = sandbox.metrics_file();
    assert!(!metrics_file.exists());

    let output = sandbox.run(&["plan", "-W", "fixtures/matrix-needs.yml"]);
    assert!(output.status.success());

    let contents = std::fs::read_to_string(&metrics_file).expect("metrics file must exist");
    let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1);
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["command"], "plan");
    let stage_names: Vec<&str> = record["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(stage_names, vec!["parse", "eval", "plan"]);
}

#[test]
fn plan_fails_actionably_when_its_required_metrics_record_cannot_be_written() {
    let sandbox = sandbox_with_fixture();
    std::fs::create_dir_all(sandbox.metrics_file()).expect("create blocking metrics directory");

    let output = sandbox.run(&["plan", "-W", "fixtures/matrix-needs.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("failed to open metrics file"), "{stderr}");
    assert!(
        stderr.contains("fix the listed path's ownership and permissions"),
        "{stderr}"
    );

    // The default store resolves HOME once, then refuses symlinks in each
    // `.litci/metrics/runs.ndjson` component. A pre-planted component must
    // not redirect Greenlit's append into another local file.
    let linked = sandbox_with_fixture();
    let external = tempfile::tempdir().expect("external metrics directory");
    let external_litci = external.path().join("litci");
    std::fs::create_dir_all(external_litci.join("metrics")).expect("create external metrics tree");
    let redirected = external_litci.join("metrics/runs.ndjson");
    std::fs::write(&redirected, "must remain unchanged\n").expect("write redirected sentinel");
    let metrics_file = linked.metrics_file();
    let litci_dir = metrics_file
        .parent()
        .and_then(std::path::Path::parent)
        .expect(".litci path");
    symlink(&external_litci, litci_dir).expect("link .litci directory");

    let output = linked.run(&["plan", "-W", "fixtures/matrix-needs.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("symbolic link or special file"), "{stderr}");
    assert!(
        stderr.contains("replace the listed symbolic link"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(redirected).expect("read redirected sentinel"),
        "must remain unchanged\n"
    );
}
