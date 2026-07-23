//! Typed and actionable workflow-dispatch input failures.

use super::common::*;
use super::support;

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
