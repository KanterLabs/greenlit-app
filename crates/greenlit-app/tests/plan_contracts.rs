//! Public `litci plan` contracts not owned by the matrix-needs fixture:
//! graph diagnostics, every remaining recognized v0 rejection, synthetic
//! pull-request/manual-dispatch behavior, environment layering, static skip
//! propagation, zero-leg matrices, and JSON/stderr diagnostic separation.

pub mod support;

use support::Sandbox;

const RICH_WORKFLOW: &str = r#"on:
  pull_request:
  workflow_dispatch:
    inputs:
      required_text:
        required: true
        type: string
      enabled:
        required: true
        type: boolean
      count:
        type: number
        default: 3
      mode:
        type: choice
        options: [fast, slow]
        default: slow
env:
  LEVEL: workflow
jobs:
  workflow_env:
    runs-on: ubuntu-latest
    steps:
      - id: workflow_layer
        run: echo ${{ env.LEVEL }}
  skipped:
    runs-on: ubuntu-latest
    if: false
    steps:
      - run: echo skipped
  dependent:
    needs: [skipped, skipped]
    runs-on: ubuntu-latest
    env:
      LEVEL: job
    steps:
      - id: job_layer
        run: echo ${{ env.LEVEL }}
      - id: step_layer
        env:
          LEVEL: step
        run: echo ${{ env.LEVEL }}
      - id: no_if
        run: echo implicit
      - id: status_if
        if: always()
        run: echo explicit
  rescued:
    needs: skipped
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo rescued
  zero:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        axis: [only]
        exclude:
          - axis: only
          - axis: missing
    steps:
      - run: echo never
  supported_label_list:
    runs-on: [ubuntu-latest]
    steps:
      - run: echo supported
  runner_source:
    runs-on: ubuntu-latest
    outputs:
      label: ${{ steps.select.outputs.label }}
    steps:
      - id: select
        run: echo "label=ubuntu-22.04" >> "$GITHUB_OUTPUT"
  deferred_runner:
    needs: runner_source
    runs-on: ${{ needs.runner_source.outputs.label }}
    steps:
      - run: echo deferred runner
  pr_shape:
    runs-on: ubuntu-latest
    if: github.event_name != 'pull_request' || (github.event.pull_request.number == 1 && github.base_ref == 'main' && github.head_ref == 'main')
    steps:
      - run: echo pull-request
  dispatch_shape:
    runs-on: ubuntu-latest
    if: github.event_name != 'workflow_dispatch' || (inputs.required_text == 'hello' && inputs.enabled == true && inputs.count == 2.5 && inputs.mode == 'fast' && github.event.inputs.enabled == 'true' && github.event.inputs.count == '2.5')
    steps:
      - run: echo dispatch
"#;

fn sandbox_with_workflow(source: &str) -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write("contracts.yml", source);
    sandbox.init_git();
    sandbox
}

fn plan_json(sandbox: &Sandbox, extra_args: &[&str]) -> (serde_json::Value, String, String) {
    let mut args = vec!["plan", "-W", "contracts.yml", "--json"];
    args.extend_from_slice(extra_args);
    let output = sandbox.run(&args);
    assert!(
        output.status.success(),
        "plan failed: {}",
        support::stderr_text(&output)
    );
    let plan = serde_json::from_slice(&output.stdout).expect("plan stdout must be one JSON value");
    (
        plan,
        support::stdout_text(&output),
        support::stderr_text(&output),
    )
}

fn job<'a>(plan: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    plan["jobs"]
        .as_array()
        .expect("jobs array")
        .iter()
        .find(|job| job["id"] == id)
        .unwrap_or_else(|| panic!("job '{id}' missing from plan"))
}

fn step<'a>(job: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    job["steps"]
        .as_array()
        .expect("steps array")
        .iter()
        .find(|step| step["id"] == id)
        .unwrap_or_else(|| panic!("step '{id}' missing from plan"))
}

#[test]
fn graph_failures_name_the_jobs_and_render_the_exact_span_and_fix() {
    let rows = [
        (
            "unknown need",
            "on: push\njobs:\n  consumer:\n    needs: missing\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            [
                "contracts.yml:4:12",
                "job 'consumer' needs unknown job 'missing'",
                "fix: fix the `needs:` entry",
            ],
        ),
        (
            "named cycle",
            "on: push\njobs:\n  alpha:\n    needs: beta\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo alpha\n  beta:\n    needs: alpha\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo beta\n",
            [
                "contracts.yml:3:3",
                "dependency cycle: alpha -> beta -> alpha",
                "fix: break the cycle",
            ],
        ),
    ];

    for (name, source, expected) in rows {
        let sandbox = sandbox_with_workflow(source);
        let output = sandbox.run(&["plan", "-W", "contracts.yml"]);
        assert!(!output.status.success(), "row '{name}' must fail");
        let stderr = support::stderr_text(&output);
        for fragment in expected {
            assert!(stderr.contains(fragment), "row '{name}': {stderr}");
        }
    }
}

#[test]
fn every_remaining_recognized_v0_construct_fails_at_its_authored_key() {
    let rows = [
        (
            "workflow concurrency",
            "on: push\nconcurrency: deploy\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            "concurrency",
            "contracts.yml:2:1",
        ),
        (
            "job concurrency",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    concurrency: deploy\n    steps:\n      - run: echo hi\n",
            "concurrency",
            "contracts.yml:5:5",
        ),
        (
            "environment",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    environment: production\n    steps:\n      - run: echo hi\n",
            "environment",
            "contracts.yml:5:5",
        ),
        (
            "reusable call job",
            "on: push\njobs:\n  call:\n    uses: ./.github/workflows/reusable.yml\n",
            "reusable workflow call (jobs.<id>.uses)",
            "contracts.yml:4:5",
        ),
    ];

    for (name, source, construct, location) in rows {
        let sandbox = sandbox_with_workflow(source);
        let output = sandbox.run(&["plan", "-W", "contracts.yml"]);
        assert!(!output.status.success(), "row '{name}' must fail");
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(location), "row '{name}': {stderr}");
        assert!(
            stderr.contains(&format!("{construct}: not in v0")),
            "row '{name}': {stderr}"
        );
        assert!(
            stderr.contains("fix: remove or restructure the workflow"),
            "row '{name}': {stderr}"
        );
    }
}

#[test]
fn pull_request_event_exposes_the_documented_synthetic_shape() {
    let sandbox = sandbox_with_workflow(RICH_WORKFLOW);
    let (plan, _, _) = plan_json(&sandbox, &["-e", "pull_request"]);

    assert_eq!(plan["event_name"], "pull_request");
    assert_eq!(job(&plan, "pr_shape")["condition"]["evaluation"], "static");
    assert_eq!(job(&plan, "pr_shape")["condition"]["value"], true);
}

#[test]
fn dispatch_plan_pins_typed_inputs_layers_skips_zero_legs_and_json_diagnostics() {
    let sandbox = sandbox_with_workflow(RICH_WORKFLOW);
    let (plan, stdout, stderr) = plan_json(
        &sandbox,
        &[
            "-e",
            "workflow_dispatch",
            "--input",
            "required_text=hello",
            "--input",
            "enabled=true",
            "--input",
            "count=2.5",
            "--input",
            "mode=fast",
        ],
    );

    assert_eq!(plan["event_name"], "workflow_dispatch");
    assert_eq!(
        job(&plan, "dispatch_shape")["condition"]["value"],
        true,
        "typed inputs and github.event.inputs strings must both compare correctly"
    );

    assert_eq!(plan["env"]["LEVEL"]["value"], "workflow");
    assert_eq!(
        step(job(&plan, "workflow_env"), "workflow_layer")["kind"]["script"]["value"],
        "echo workflow"
    );
    let dependent = job(&plan, "dependent");
    assert_eq!(dependent["env"]["LEVEL"]["value"], "job");
    assert_eq!(
        step(dependent, "job_layer")["kind"]["script"]["value"],
        "echo job"
    );
    assert_eq!(
        step(dependent, "step_layer")["env"]["LEVEL"]["value"],
        "step"
    );
    assert_eq!(
        step(dependent, "step_layer")["kind"]["script"]["value"],
        "echo step"
    );

    let skipped = job(&plan, "skipped");
    assert_eq!(skipped["implicit_status_gate"], true);
    assert_eq!(skipped["skip"]["kind"], "condition-false");
    assert_eq!(dependent["needs"], serde_json::json!(["skipped"]));
    assert_eq!(dependent["implicit_status_gate"], true);
    assert_eq!(dependent["skip"]["kind"], "need-skipped");
    assert_eq!(dependent["skip"]["need"], "skipped");
    assert_eq!(step(dependent, "no_if")["implicit_status_gate"], true);
    assert_eq!(step(dependent, "status_if")["implicit_status_gate"], false);

    let rescued = job(&plan, "rescued");
    assert_eq!(rescued["implicit_status_gate"], false);
    assert!(rescued["skip"].is_null());
    assert_eq!(rescued["condition"]["evaluation"], "deferred");

    let zero = job(&plan, "zero");
    assert_eq!(zero["strategy"]["is_matrix"], true);
    assert_eq!(zero["strategy"]["legs"], serde_json::json!([]));
    assert_eq!(zero["legs"], serde_json::json!([]));
    assert!(zero["runner"].is_null());

    let static_runner = &job(&plan, "supported_label_list")["runner"];
    assert_eq!(static_runner["source"], "ubuntu-latest");
    assert_eq!(static_runner["evaluation"], "static");
    assert_eq!(static_runner["value"], "ubuntu-24.04");
    assert_eq!(static_runner["span"], "contracts.yml:65:15");

    let deferred_runner = &job(&plan, "deferred_runner")["runner"];
    assert_eq!(
        deferred_runner,
        &serde_json::json!({
            "span": "contracts.yml:77:14",
            "source": "${{ needs.runner_source.outputs.label }}",
            "evaluation": "deferred",
            "residual": "needs.runner_source.outputs.label",
            "defers_on": [{
                "kind": "needs-output",
                "job": "runner_source",
                "output": "label"
            }]
        })
    );

    let lint_kinds = plan["lints"]
        .as_array()
        .expect("lint array")
        .iter()
        .map(|lint| lint["kind"].as_str().expect("lint kind"))
        .collect::<Vec<_>>();
    assert!(lint_kinds.contains(&"duplicate-needs"));
    assert!(lint_kinds.contains(&"dead-exclude"));
    assert!(!stdout.contains("warning:"));
    assert!(stderr.contains("warning: contracts.yml:"), "{stderr}");
    assert!(
        stderr.contains("duplicate `needs` entry 'skipped'"),
        "{stderr}"
    );
    assert!(
        stderr.contains("exclude` entry matched no surviving"),
        "{stderr}"
    );
    assert!(stderr.contains("stage timings (plan):"), "{stderr}");

    let human = sandbox.run(&[
        "plan",
        "-W",
        "contracts.yml",
        "-e",
        "workflow_dispatch",
        "--input",
        "required_text=hello",
        "--input",
        "enabled=true",
        "--input",
        "count=2.5",
        "--input",
        "mode=fast",
    ]);
    assert!(human.status.success());
    assert!(
        support::stdout_text(&human).contains(
            "runner: deferred <- needs.runner_source.outputs.label (defers on: needs.runner_source.outputs.label)"
        )
    );
}

#[test]
fn dispatch_input_failures_are_typed_named_and_actionable() {
    let rows: &[(&str, &[&str], &[&str])] = &[
        (
            "input on pull request",
            &["-e", "pull_request", "--input", "required_text=hello"],
            &[
                "inputs are only valid for `workflow_dispatch`",
                "fix: select -e workflow_dispatch, or remove the --input arguments",
            ],
        ),
        (
            "missing required boolean",
            &["-e", "workflow_dispatch", "--input", "required_text=hello"],
            &[
                "contracts.yml:9:9",
                "required workflow_dispatch input 'enabled' has no value",
                "fix: pass --input NAME=VALUE",
            ],
        ),
        (
            "unknown input",
            &[
                "-e",
                "workflow_dispatch",
                "--input",
                "required_text=hello",
                "--input",
                "enabled=true",
                "--input",
                "mystery=value",
            ],
            &[
                "workflow_dispatch input 'mystery' is not declared",
                "declared inputs: count, enabled, mode, required_text",
                "fix: use a workflow_dispatch input name declared by this workflow",
            ],
        ),
        (
            "invalid boolean",
            &[
                "-e",
                "workflow_dispatch",
                "--input",
                "required_text=hello",
                "--input",
                "enabled=maybe",
            ],
            &[
                "workflow_dispatch input 'enabled' must be `true` or `false`",
                "fix: pass --input NAME=VALUE using the declared input type/options",
            ],
        ),
        (
            "invalid number",
            &[
                "-e",
                "workflow_dispatch",
                "--input",
                "required_text=hello",
                "--input",
                "enabled=true",
                "--input",
                "count=NaN",
            ],
            &[
                "workflow_dispatch input 'count' must be a finite number",
                "fix: pass --input NAME=VALUE using the declared input type/options",
            ],
        ),
        (
            "invalid choice",
            &[
                "-e",
                "workflow_dispatch",
                "--input",
                "required_text=hello",
                "--input",
                "enabled=true",
                "--input",
                "mode=turbo",
            ],
            &[
                "workflow_dispatch input 'mode' must be one of: fast, slow",
                "fix: pass --input NAME=VALUE using the declared input type/options",
            ],
        ),
    ];

    for (name, extra_args, expected) in rows {
        let sandbox = sandbox_with_workflow(RICH_WORKFLOW);
        let mut args = vec!["plan", "-W", "contracts.yml"];
        args.extend_from_slice(extra_args);
        let output = sandbox.run(&args);
        assert!(!output.status.success(), "row '{name}' must fail");
        let stderr = support::stderr_text(&output);
        for fragment in *expected {
            assert!(stderr.contains(fragment), "row '{name}': {stderr}");
        }
    }
}

#[test]
fn static_step_controls_require_githubs_documented_types_and_timeout_range() {
    let valid = sandbox_with_workflow(
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - id: bounded\n        continue-on-error: ${{ fromJSON('false') }}\n        timeout-minutes: ${{ fromJSON('360') }}\n        run: echo valid\n",
    );
    let (plan, _, _) = plan_json(&valid, &[]);
    let bounded = step(job(&plan, "build"), "bounded");
    assert_eq!(bounded["continue_on_error"]["value"], false);
    assert_eq!(bounded["timeout_minutes"]["value"], 360.0);

    let rows = [
        (
            "string continue-on-error",
            "continue-on-error: ${{ 'false' }}",
            "contracts.yml:7:28",
            "static expression must evaluate to a boolean",
        ),
        (
            "string timeout",
            "timeout-minutes: ${{ '3' }}",
            "contracts.yml:7:26",
            "static expression must evaluate to an integer from 1 through 360 minutes",
        ),
        (
            "zero timeout",
            "timeout-minutes: 0",
            "contracts.yml:7:26",
            "static expression must evaluate to an integer from 1 through 360 minutes",
        ),
        (
            "fractional timeout",
            "timeout-minutes: 1.5",
            "contracts.yml:7:26",
            "static expression must evaluate to an integer from 1 through 360 minutes",
        ),
        (
            "over-limit timeout",
            "timeout-minutes: 361",
            "contracts.yml:7:26",
            "static expression must evaluate to an integer from 1 through 360 minutes",
        ),
    ];

    for (name, field, location, message) in rows {
        let source = format!(
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo invalid\n        {field}\n"
        );
        let sandbox = sandbox_with_workflow(&source);
        let output = sandbox.run(&["plan", "-W", "contracts.yml", "--json"]);
        assert!(!output.status.success(), "row '{name}' must fail");
        assert!(output.stdout.is_empty(), "row '{name}' wrote plan JSON");
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(location), "row '{name}': {stderr}");
        assert!(stderr.contains(message), "row '{name}': {stderr}");
        assert!(
            stderr.contains("fix: fix the expression referenced above"),
            "row '{name}': {stderr}"
        );
    }
}
