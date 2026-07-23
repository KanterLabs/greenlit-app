//! Static step-control types and workflow expression memory budget.

use super::common::*;
use super::support;

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
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(location), "row '{name}': {stderr}");
        assert!(stderr.contains(message), "row '{name}': {stderr}");
        assert!(
            stderr.contains("fix: fix the expression referenced above"),
            "row '{name}': {stderr}"
        );
    }
}

#[test]
fn workflow_expressions_use_the_runners_template_memory_budget() {
    // A bare Actions expression SDK evaluation defaults to 1 MiB, but every
    // expression reached through TemplateToken inherits the workflow
    // template's 10 MiB budget. Repeating this 2,048-byte argument 513 times
    // crosses only the bare-SDK construction limit; planning must therefore
    // succeed and resolve the comparison to false.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTObjectTemplating/ObjectTemplating/Tokens/TemplateToken.cs#L52-L65
    let pattern = "{0}".repeat(513);
    let argument = "x".repeat(1_011);
    let source = format!(
        "on: push\njobs:\n  budget:\n    runs-on: ubuntu-latest\n    if: format('{pattern}', '{argument}') == ''\n    steps:\n      - run: echo unreachable\n"
    );
    let sandbox = sandbox_with_workflow(&source);
    let (plan, _stdout, _stderr) = plan_json(&sandbox, &[]);

    assert_eq!(job(&plan, "budget")["condition"]["evaluation"], "static");
    assert_eq!(job(&plan, "budget")["condition"]["value"], false);
    assert_eq!(job(&plan, "budget")["skip"]["kind"], "condition-false");
}
