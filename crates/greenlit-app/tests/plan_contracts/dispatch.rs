//! Rich workflow-dispatch plan contract.

use super::common::*;
use super::support;

#[test]
fn dispatch_plan_pins_typed_inputs_layers_skips_zero_legs_and_json_diagnostics() {
    let sandbox = sandbox_with_workflow(RICH_WORKFLOW);
    let (plan, stdout, stderr) = plan_json(&sandbox, &["-e", "workflow_dispatch"]);

    assert_eq!(plan["event_name"], "workflow_dispatch");
    assert_eq!(
        plan["run_name"],
        serde_json::json!({
            "span": "contracts.yml:202:11",
            "source": "Run ${{ inputs.mode }} by ${{ github.actor }}",
            "evaluation": "static",
            "value": "Run fast by litci tests"
        })
    );
    assert_eq!(
        job(&plan, "dispatch_shape")["condition"]["value"],
        true,
        "typed inputs and deterministic synthetic github fields must compare correctly"
    );

    let password = &job(&plan, "render_fields")["container"]["credentials"]["password"];
    assert_eq!(password["evaluation"], "static");
    assert_eq!(password["source"], "[masked]");
    assert_eq!(password["value"], "[masked]");
    assert!(!stdout.contains("fixture-password"));
    assert!(!stderr.contains("fixture-password"));

    let runtime_context = step(job(&plan, "github_runtime"), "runtime_context");
    let runtime_fields = &runtime_context["env"]["RUNTIME_FIELDS"];
    assert_eq!(runtime_fields["evaluation"], "deferred");
    let properties = runtime_fields["defers_on"]
        .as_array()
        .expect("runtime github dependencies")
        .iter()
        .filter(|reason| reason["kind"] == "github-context")
        .map(|reason| {
            reason["property"]
                .as_str()
                .expect("property-specific github dependency")
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        properties,
        std::collections::BTreeSet::from([
            "action",
            "action_path",
            "action_ref",
            "action_repository",
            "action_status",
            "actor_id",
            "env",
            "event_path",
            "job",
            "path",
            "ref_protected",
            "repositoryUrl",
            "repository_id",
            "repository_owner_id",
            "retention_days",
            "run_attempt",
            "run_id",
            "run_number",
            "secret_source",
            "token",
            "workspace",
        ])
    );
    assert_eq!(
        runtime_context["env"]["WHOLE_GITHUB"]["defers_on"],
        serde_json::json!([{ "kind": "github-context" }])
    );

    let env_runtime = job(&plan, "env_runtime");
    let initial_env = step(env_runtime, "initial_env");
    assert_eq!(
        initial_env["env"]["STATIC_COMPUTED"]["evaluation"],
        "static"
    );
    assert_eq!(
        initial_env["env"]["STATIC_COMPUTED"]["value"], "job",
        "a fully-static computed env key resolves to its exact name"
    );
    assert_eq!(initial_env["env"]["DYNAMIC_KEY"]["evaluation"], "deferred");
    assert_eq!(
        initial_env["env"]["DYNAMIC_KEY"]["defers_on"],
        serde_json::json!([
            {
                "kind": "needs-output",
                "job": "runner_source",
                "output": "label"
            },
            {"kind": "dynamic-env", "name": "*"}
        ]),
        "only a genuinely runtime-dependent key retains wildcard env uncertainty"
    );
    assert_eq!(initial_env["env"]["WRONG_CASE"]["evaluation"], "static");
    assert_eq!(
        initial_env["env"]["WRONG_CASE"]["value"], "",
        "Linux env-context lookup is case-sensitive, and a differently cased undeclared name is absent before any executable predecessor"
    );
    assert_eq!(initial_env["env"]["FIRST_UNSET"]["evaluation"], "static");
    assert_eq!(
        initial_env["env"]["FIRST_UNSET"]["value"], "",
        "an undeclared name is statically absent before any executable predecessor"
    );
    assert_eq!(
        initial_env["kind"]["script"]["value"],
        "echo initial=job unset="
    );

    let deferred_after_runnable = step(env_runtime, "deferred_after_runnable");
    assert_eq!(
        deferred_after_runnable["kind"]["script"]["evaluation"],
        "deferred"
    );
    assert_eq!(
        deferred_after_runnable["kind"]["script"]["defers_on"],
        serde_json::json!([
            {
                "kind": "needs-output",
                "job": "runner_source",
                "output": "label"
            },
            {"kind": "dynamic-env", "name": "DEFERRED"}
        ]),
        "a declared deferred value retains its source dependency while a runnable predecessor adds GITHUB_ENV mutability"
    );
    let after_mutation = step(env_runtime, "after_mutation");
    assert_eq!(after_mutation["kind"]["script"]["evaluation"], "deferred");
    assert_eq!(
        after_mutation["kind"]["script"]["defers_on"],
        serde_json::json!([{"kind": "dynamic-env", "name": "FOO"}]),
        "a fully-static computed key preserves exact-name GITHUB_ENV mutability instead of becoming wildcard uncertainty"
    );
    let step_override = step(env_runtime, "step_override");
    assert_eq!(step_override["env"]["FOO"]["value"], "fixed");
    assert_eq!(step_override["kind"]["script"]["evaluation"], "static");
    assert_eq!(step_override["kind"]["script"]["value"], "echo fixed");

    assert_eq!(plan["env"]["LEVEL"]["value"], "workflow");
    assert_eq!(
        step(job(&plan, "workflow_env"), "workflow_layer")["kind"]["script"]["value"],
        "echo workflow"
    );
    let dependent = job(&plan, "dependent");
    assert_eq!(
        dependent["permissions"],
        serde_json::json!({
            "kind": "scoped",
            "scopes": {"contents": "read", "actions": "write"}
        }),
        "jobs without an override inherit the workflow declaration"
    );
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
    assert_eq!(zero["strategy"]["matrix"]["evaluation"], "static");
    assert_eq!(zero["strategy"]["matrix"]["legs"], serde_json::json!([]));
    assert_eq!(zero["legs"], serde_json::json!([]));
    assert!(zero["runner"].is_null());

    let static_runner = &job(&plan, "supported_label_list")["runner"];
    assert_eq!(static_runner["source"], "ubuntu-latest");
    assert_eq!(static_runner["evaluation"], "static");
    assert_eq!(static_runner["value"], "ubuntu-24.04");
    assert_eq!(static_runner["span"], "contracts.yml:67:15");

    let deferred_runner = &job(&plan, "deferred_runner")["runner"];
    assert_eq!(
        deferred_runner,
        &serde_json::json!({
            "span": "contracts.yml:79:14",
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

    assert_eq!(
        job(&plan, "render_fields")["permissions"],
        serde_json::json!({
            "kind": "scoped",
            "scopes": {"contents": "write"}
        }),
        "a job declaration replaces the workflow declaration instead of merging"
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

    let human = sandbox.run(&["plan", "-W", "contracts.yml", "-e", "workflow_dispatch"]);
    assert!(human.status.success());
    let human_stdout = support::stdout_text(&human);
    let human_stderr = support::stderr_text(&human);
    assert!(!human_stdout.contains("fixture-password"));
    assert!(!human_stderr.contains("fixture-password"));
    let metrics = std::fs::read_to_string(sandbox.metrics_file()).expect("plan metrics history");
    assert!(!metrics.contains("fixture-password"));
    let expected_human_fields = [
        "plan schema: 1",
        "run name: static(\"Run fast by litci tests\") <- Run ${{ inputs.mode }} by ${{ github.actor }}",
        "defaults.run:\n  shell: static(\"bash\") <- bash\n  working-directory: static(\"workflow-work\") <- workflow-work",
        "permissions: {\"kind\":\"scoped\",\"scopes\":{\"contents\":\"read\",\"actions\":\"write\"}}",
        "permissions: {\"kind\":\"scoped\",\"scopes\":{\"contents\":\"write\"}}",
        "runner: deferred <- needs.runner_source.outputs.label (defers on: needs.runner_source.outputs.label)",
        "Render workflow_dispatch [wave 0] needs: (none)",
        "name: static(\"Render workflow_dispatch\") <- Render ${{ github.event_name }}",
        "image: static(\"alpine:3.20\") <- alpine:3.20",
        "username: static(\"fixture-user\") <- fixture-user",
        "password: static([masked])",
        "CONTAINER_EVENT: static(\"workflow_dispatch\") <- ${{ github.event_name }}",
        "- static(\"8080:80\") <- 8080:80",
        "- static(\"/tmp:/tmp\") <- /tmp:/tmp",
        "options: static(\"--cpus 1\") <- --cpus 1",
        "SERVICE_EVENT: static(\"workflow_dispatch\") <- ${{ github.event_name }}",
        "JOB_EVENT: static(\"workflow_dispatch\") <- ${{ github.event_name }}",
        "static-output: static(\"rendered\") <- rendered",
        "deferred-output: deferred <- steps.rich.outputs.value (defers on: steps.rich.outputs.value)",
        "name: static(\"Rich workflow_dispatch\") <- Rich ${{ github.event_name }}",
        "if: static(true) <- github.event_name == 'workflow_dispatch'",
        "kind: run",
        "script: static(\"echo first\\necho second\\n\") <- echo first\\necho second\\n",
        "shell: static(\"bash\") <- bash",
        "STEP_EVENT: static(\"workflow_dispatch\") <- ${{ github.event_name }}",
        "working-directory: static(\"step-work\") <- step-work",
        "continue-on-error: static(false) <- ${{ fromJSON('false') }}",
        "timeout-minutes: static(30) <- ${{ fromJSON('30') }}",
        "kind: uses",
        "reference: static(\"actions/checkout@0123456789abcdef0123456789abcdef01234567\")",
        "ref: static(\"refs/heads/main\") <- ${{ github.ref }}",
        "fetch-depth: static(\"1\") <- 1",
        "leg 0 \"Matrix stable\"",
        "image: static(\"alpine:stable\") <- alpine:${{ matrix.channel }}",
        "image: static(\"redis:stable\") <- redis:${{ matrix.channel }}",
        "MATRIX_CHANNEL: static(\"stable\") <- ${{ matrix.channel }}",
        "selected: static(\"stable\") <- ${{ matrix.channel }}",
    ];
    for expected in expected_human_fields {
        assert!(
            human_stdout.contains(expected),
            "missing human-plan field `{expected}`:\n{human_stdout}"
        );
    }
}
