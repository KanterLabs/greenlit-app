//! Matrix-needs integration coverage for Phase 12 variable containment.

use super::support;
use super::{MATRIX_NEEDS_PATH, deploy_condition_value, sandbox_with_deploy_condition};

#[test]
fn explicit_plan_variables_are_quarantined_without_retention() {
    const FIRST: &str = "ghp_GL_STAB_UNUSED_VAR_FIRST_027";
    for value in [FIRST, "shell", "variable.context"] {
        let sandbox = sandbox_with_deploy_condition("github.event_name == 'push'");
        let assignment = format!("UNUSED={value}");
        let output = sandbox.run(&[
            "plan",
            "-W",
            MATRIX_NEEDS_PATH,
            "--json",
            "--var",
            assignment.as_str(),
        ]);
        assert_eq!(output.status.code(), Some(1));
        let stdout = support::stdout_text(&output);
        let stderr = support::stderr_text(&output);
        assert!(stdout.is_empty(), "{stdout}");
        assert!(
            stderr.contains("stabilization Phase 16")
                && stderr.contains("explicit variables have not completed"),
            "{stderr}"
        );
        assert!(!stdout.contains(value), "{stdout}");
        assert!(!stderr.contains(value), "{stderr}");
        assert!(
            !sandbox.metrics_file().exists(),
            "rejected credential-bearing plan argument created metrics"
        );
    }
}

#[test]
fn unknown_context_property_is_falsey_in_the_matrix_needs_plan() {
    let sandbox = sandbox_with_deploy_condition("github.missing");
    let output = sandbox.run(&["plan", "-W", MATRIX_NEEDS_PATH, "--json"]);
    assert!(!deploy_condition_value(&output));
}
