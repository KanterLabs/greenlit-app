//! Integration tests: v0 scope rejections (`PHASE-1-engine-core.md` exit
//! criterion 4 and `greenlit-v0-spec.md`'s Out list), plus unsupported
//! `runs-on:` labels (with the accepted stable-x64 list).

pub mod support;

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
fn every_unsupported_runner_form_is_rejected_with_the_accepted_list() {
    let rows = [
        ("Windows", UNSUPPORTED_RUNNER_FIXTURE, "windows-latest"),
        (
            "macOS",
            "on: push\njobs:\n  build:\n    runs-on: macos-latest\n    steps:\n      - run: echo hi\n",
            "macos-latest",
        ),
        (
            "ARM",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-24.04-arm\n    steps:\n      - run: echo hi\n",
            "ubuntu-24.04-arm",
        ),
        (
            "preview/slim",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-slim\n    steps:\n      - run: echo hi\n",
            "ubuntu-slim",
        ),
        (
            "self-hosted labels",
            "on: push\njobs:\n  build:\n    runs-on: [self-hosted, linux, x64]\n    steps:\n      - run: echo hi\n",
            "multi-label `runs-on: [...]` self-hosted selection",
        ),
        (
            "runner group",
            "on: push\njobs:\n  build:\n    runs-on:\n      group: production-runners\n      labels: [linux, x64]\n    steps:\n      - run: echo hi\n",
            "custom `runs-on: { group: ... }` runner group",
        ),
        (
            "larger runner",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-24.04-16core\n    steps:\n      - run: echo hi\n",
            "ubuntu-24.04-16core",
        ),
    ];

    for (name, source, rejected) in rows {
        let sandbox = Sandbox::new();
        sandbox.write("fixtures/unsupported-runner.yml", source);
        sandbox.init_git();

        let output = sandbox.run(&["plan", "-W", "fixtures/unsupported-runner.yml"]);
        assert!(
            !output.status.success(),
            "row '{name}' unexpectedly planned"
        );
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(rejected), "row '{name}': {stderr}");
        assert!(
            stderr.contains("fixtures/unsupported-runner.yml:"),
            "row '{name}' lacks a source location: {stderr}"
        );
        for accepted in ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04", "homelab"] {
            assert!(stderr.contains(accepted), "row '{name}': {stderr}");
        }
        assert!(stderr.contains("fix:"), "row '{name}': {stderr}");
    }
}

#[test]
fn oidc_write_permissions_are_rejected_while_none_is_harmless() {
    let sandbox = Sandbox::new();
    sandbox.init_git();

    for (name, permission) in [
        ("workflow scope", "permissions:\n  id-token: write\n"),
        ("write-all", "permissions: write-all\n"),
    ] {
        sandbox.write(
            ".github/workflows/oidc.yml",
            &format!(
                "on: push\n{permission}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo build\n"
            ),
        );
        let output = sandbox.run(&["plan", "-W", ".github/workflows/oidc.yml"]);
        assert!(!output.status.success(), "{name} unexpectedly planned");
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains("OIDC"), "{name}: {stderr}");
        assert!(stderr.contains("id-token: write"), "{name}: {stderr}");
        assert!(stderr.contains("not in v0"), "{name}: {stderr}");
        assert!(stderr.contains("fix:"), "{name}: {stderr}");
    }

    sandbox.write(
        ".github/workflows/oidc.yml",
        "on: push\npermissions:\n  id-token: none\njobs:\n  build:\n    permissions:\n      id-token: none\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo build\n",
    );
    let output = sandbox.run(&["plan", "-W", ".github/workflows/oidc.yml"]);
    assert!(
        output.status.success(),
        "id-token: none must remain harmless: {}",
        support::stderr_text(&output)
    );

    // Job-level permissions replace, rather than merge with, workflow-level
    // permissions. An explicit job map therefore neutralizes inherited OIDC.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idpermissions
    sandbox.write(
        ".github/workflows/oidc.yml",
        "on: push\npermissions:\n  id-token: write\njobs:\n  build:\n    permissions:\n      contents: read\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo build\n",
    );
    let output = sandbox.run(&["plan", "-W", ".github/workflows/oidc.yml"]);
    assert!(
        output.status.success(),
        "job-level permission replacement must disable inherited OIDC: {}",
        support::stderr_text(&output)
    );

    sandbox.write(
        ".github/workflows/oidc.yml",
        "on: push\njobs:\n  build:\n    permissions:\n      id-token: write\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo build\n",
    );
    let output = sandbox.run(&["plan", "-W", ".github/workflows/oidc.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("OIDC"));
    assert!(stderr.contains("not in v0"));
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
fn yaml_depth_limit_is_actionable_before_model_recursion() {
    let sandbox = Sandbox::new();
    let nested = format!("on: {}x{}\njobs: {{}}\n", "[".repeat(51), "]".repeat(51));
    sandbox.write("deep.yml", &nested);
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "deep.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("deep.yml:1:"), "{stderr}");
    assert!(stderr.contains("depth of 50"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
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
