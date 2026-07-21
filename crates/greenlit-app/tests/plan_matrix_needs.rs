//! Integration tests: `litci plan` against `fixtures/matrix-needs.yml`
//! (`TESTING.md` integration-test class; `PHASE-1-engine-core.md` exit
//! criterion 2). Asserts the *resolved plan structure* -- job count and
//! order, matrix expansion, output/needs wiring, static vs. deferred `if:`
//! markers -- not just a zero exit code.

mod support;

use support::Sandbox;

/// Embedded at compile time so the checked-in fixture is exactly what every
/// test below plans against, with no runtime dependency on the test
/// runner's own working directory.
const MATRIX_NEEDS_FIXTURE: &str = include_str!("../../../fixtures/matrix-needs.yml");

/// Writes the fixture into a fresh sandbox and returns it, ready to `plan`.
fn sandbox_with_fixture() -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write("fixtures/matrix-needs.yml", MATRIX_NEEDS_FIXTURE);
    sandbox.init_git();
    sandbox
}

fn plan_json(sandbox: &Sandbox) -> serde_json::Value {
    let output = sandbox.run(&["plan", "-W", "fixtures/matrix-needs.yml", "--json"]);
    assert!(
        output.status.success(),
        "plan failed: {}",
        support::stderr_text(&output)
    );
    serde_json::from_slice(&output.stdout).expect("plan --json stdout must be valid JSON")
}

#[test]
fn job_count_and_declaration_order_are_preserved() {
    let sandbox = sandbox_with_fixture();
    let plan = plan_json(&sandbox);
    let jobs = plan["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 3);
    let ids: Vec<&str> = jobs.iter().map(|j| j["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["build", "test", "deploy"]);
    assert_eq!(
        plan["topo_order"],
        serde_json::json!(["build", "test", "deploy"])
    );
}

#[test]
fn matrix_expands_include_and_exclude_to_two_legs() {
    let sandbox = sandbox_with_fixture();
    let plan = plan_json(&sandbox);
    let build = &plan["jobs"][0];
    let legs = build["strategy"]["legs"].as_array().expect("legs array");
    // `exclude: [{channel: beta}]` drops both beta combos (partial-key
    // match); `include` then adds `canary: true` to the one surviving combo
    // it fits (`os: ubuntu-24.04, channel: stable`) rather than creating a
    // third, standalone leg.
    assert_eq!(legs.len(), 2);
    assert_eq!(legs[0]["values"]["os"], "ubuntu-22.04");
    assert_eq!(legs[0]["values"]["channel"], "stable");
    assert!(legs[0]["values"].get("canary").is_none());
    assert_eq!(legs[1]["values"]["os"], "ubuntu-24.04");
    assert_eq!(legs[1]["values"]["channel"], "stable");
    assert_eq!(legs[1]["values"]["canary"], true);
    assert_eq!(legs[0]["origin"]["kind"], "product");
    assert_eq!(legs[1]["origin"]["kind"], "product");

    // Per-leg runner resolution follows `matrix.os`, and each leg gets its
    // own resolved display name.
    assert_eq!(build["legs"][0]["runner"], "Ubuntu2204");
    assert_eq!(build["legs"][0]["name"], "build (ubuntu-22.04, stable)");
    assert_eq!(build["legs"][1]["runner"], "Ubuntu2404");
    assert_eq!(
        build["legs"][1]["name"],
        "build (ubuntu-24.04, stable, true)"
    );
    // A matrix job has no single job-level runner/condition/outputs/steps
    // (see each leg instead).
    assert!(build["runner"].is_null());
    assert!(build["condition"].is_null());
    assert!(build["outputs"]["entries"].as_object().unwrap().is_empty());
    assert!(build["steps"].as_array().unwrap().is_empty());

    // No dead-exclude lint: the `beta` exclude entry did remove something.
    assert!(
        !plan["lints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["kind"] == "dead-exclude")
    );
}

#[test]
fn needs_chain_and_wave_numbers_are_correct() {
    let sandbox = sandbox_with_fixture();
    let plan = plan_json(&sandbox);
    let jobs = plan["jobs"].as_array().unwrap();
    assert_eq!(jobs[0]["needs"], serde_json::json!([]));
    assert_eq!(jobs[0]["wave"], 0);
    assert_eq!(jobs[1]["needs"], serde_json::json!(["build"]));
    assert_eq!(jobs[1]["wave"], 1);
    assert_eq!(jobs[2]["needs"], serde_json::json!(["build", "test"]));
    assert_eq!(jobs[2]["wave"], 2);
}

#[test]
fn job_output_is_deferred_and_consumed_by_a_downstream_jobs_output() {
    let sandbox = sandbox_with_fixture();
    let plan = plan_json(&sandbox);
    // `build` is a matrix job, so its declared output lives on each leg
    // (`greenlit_engine::LegPlan::outputs`), not the job itself.
    for leg in plan["jobs"][0]["legs"].as_array().unwrap() {
        let build_output = &leg["outputs"]["entries"]["artifact-name"];
        assert_eq!(build_output["evaluation"], "deferred");
        assert_eq!(build_output["residual"], "steps.pack.outputs.name");
    }

    let deploy_output = &plan["jobs"][2]["outputs"]["entries"]["release-tag"];
    assert_eq!(deploy_output["evaluation"], "deferred");
    let defers_on = deploy_output["defers_on"].as_array().unwrap();
    assert!(defers_on.iter().any(|d| d["kind"] == "needs-output"
        && d["job"] == "build"
        && d["output"] == "artifact-name"));

    // A matrix job's outputs being read by a real dependent is flagged.
    assert!(
        plan["lints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["kind"] == "matrix-outputs-collision")
    );
}

#[test]
fn static_and_deferred_if_conditions_are_both_present() {
    let sandbox = sandbox_with_fixture();
    let plan = plan_json(&sandbox);

    // `deploy`'s job-level `if:` is statically decidable against the
    // synthetic push event.
    let deploy_condition = &plan["jobs"][2]["condition"];
    assert_eq!(deploy_condition["evaluation"], "static");
    assert_eq!(deploy_condition["value"], true);

    // `test`'s "Upload coverage" step depends on `needs.build.result`,
    // which does not exist at plan time.
    let test_steps = plan["jobs"][1]["steps"].as_array().unwrap();
    let upload_step = test_steps
        .iter()
        .find(|s| s["name"] == "Upload coverage")
        .expect("Upload coverage step");
    assert_eq!(upload_step["condition"]["evaluation"], "deferred");
    let defers_on = upload_step["condition"]["defers_on"].as_array().unwrap();
    assert!(
        defers_on
            .iter()
            .any(|d| d["kind"] == "needs-result" && d["job"] == "build")
    );
}

#[test]
fn json_output_is_byte_stable_across_runs() {
    let sandbox = sandbox_with_fixture();
    let first = sandbox.run(&["plan", "-W", "fixtures/matrix-needs.yml", "--json"]);
    let second = sandbox.run(&["plan", "-W", "fixtures/matrix-needs.yml", "--json"]);
    assert!(first.status.success());
    assert!(second.status.success());
    assert_eq!(first.stdout, second.stdout);
    // Timings/run metadata never leak into stdout.
    let stdout = support::stdout_text(&first);
    assert!(!stdout.contains("stage timings"));
    assert!(!stdout.contains("started_at_unix_ms"));
}

#[test]
fn human_tree_output_and_stderr_diagnostics_are_separated() {
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
