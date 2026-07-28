//! Typed and actionable workflow-dispatch input failures.

use super::common::*;
use super::support;

#[test]
fn dispatch_input_failures_are_typed_named_and_actionable() {
    const SENTINEL: &str = "ghp_GL_STAB_PLAN_INPUT_SENTINEL_027";

    for value in [SENTINEL, "input.dispatch"] {
        for attached in [false, true] {
            let sandbox = sandbox_with_workflow(RICH_WORKFLOW);
            let assignment = format!("required_text={value}");
            let attached_assignment = format!("--input={assignment}");
            let mut arguments = vec!["plan", "-W", "contracts.yml", "-e", "workflow_dispatch"];
            if attached {
                arguments.push(attached_assignment.as_str());
            } else {
                arguments.extend(["--input", assignment.as_str()]);
            }

            let output = sandbox.run(&arguments);
            assert_eq!(output.status.code(), Some(1));
            let stdout = support::stdout_text(&output);
            let stderr = support::stderr_text(&output);
            assert!(!stdout.contains(value), "{stdout}");
            assert!(!stderr.contains(value), "{stderr}");
            if value == SENTINEL {
                assert!(
                    stderr.contains("uncertified capability `input.dispatch`"),
                    "{stderr}"
                );
            }
            assert!(stderr.contains("stabilization Phase 16"), "{stderr}");
            assert!(
                !sandbox.home().join(".litci/runs").exists(),
                "plan created a retained run tree"
            );
            assert!(
                !sandbox.metrics_file().exists(),
                "rejected credential-bearing plan input created metrics"
            );
        }
    }
}
