//! Matrix-needs integration oracle for configuration-variable precedence,
//! naming, dynamic lookups, and missing-property semantics.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use super::support;
use super::{MATRIX_NEEDS_PATH, deploy_condition_value, sandbox_with_deploy_condition};

#[test]
fn missing_literal_var_fails_with_location_and_fix() {
    let sandbox = sandbox_with_deploy_condition("vars.MODE == 'ci'");

    let output = sandbox.run(&["plan", "-W", MATRIX_NEEDS_PATH]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("vars.MODE"));
    assert!(stderr.contains("is not set"));
    assert!(stderr.contains("fixtures/matrix-needs.yml:"));
    assert!(stderr.contains("fix:"));
    assert!(stderr.contains("--var"));
    assert!(stderr.contains(".litci/vars"));
}

#[test]
fn configuration_variable_names_follow_github_rules_and_cli_last_override_wins() {
    let sandbox = sandbox_with_deploy_condition("vars.MODE == 'ci'");

    let output = sandbox.run(&[
        "plan",
        "-W",
        MATRIX_NEEDS_PATH,
        "--json",
        "--var",
        "Mode=not-ci",
        "--var",
        "mode=ci",
    ]);
    assert!(deploy_condition_value(&output));

    let underscored_sandbox = sandbox_with_deploy_condition("vars._MODE2 == 'ci'");
    let output = underscored_sandbox.run(&[
        "plan",
        "-W",
        MATRIX_NEEDS_PATH,
        "--json",
        "--var",
        "_mode2=ci",
    ]);
    assert!(deploy_condition_value(&output));

    for name in ["1MODE", "BAD-NAME", "GITHUB_MODE", "MØDE"] {
        let assignment = format!("{name}=ci");
        let output = sandbox.run(&["plan", "-W", MATRIX_NEEDS_PATH, "--var", &assignment]);
        assert!(!output.status.success(), "{name} unexpectedly passed");
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(name), "{stderr}");
        assert!(stderr.contains("invalid --var name"), "{stderr}");
        assert!(stderr.contains("fix:"), "{stderr}");
    }

    for name in ["1MODE", "BAD-NAME", "BAD.NAME", "github_mode"] {
        let dotenv_sandbox = sandbox_with_deploy_condition("vars.MODE == 'ci'");
        dotenv_sandbox.write(".litci/vars", &format!("{name}=ci\n"));

        let output = dotenv_sandbox.run(&["plan", "-W", MATRIX_NEEDS_PATH]);
        assert!(!output.status.success(), "{name} unexpectedly passed");
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(name), "{stderr}");
        assert!(
            stderr.contains("invalid configuration-variable name"),
            "{stderr}"
        );
        assert!(stderr.contains(".litci/vars"), "{stderr}");
        assert!(stderr.contains("fix:"), "{stderr}");
    }

    // GitHub caps each configuration-variable value at 48 KiB.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/variables#limits-for-configuration-variables
    let at_limit = format!("MODE={}", "x".repeat(48 * 1024));
    let output = sandbox.run(&["plan", "-W", MATRIX_NEEDS_PATH, "--var", &at_limit]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));

    let over_limit = format!("MODE={}", "x".repeat(48 * 1024 + 1));
    let output = sandbox.run(&["plan", "-W", MATRIX_NEEDS_PATH, "--var", &over_limit]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("48 KiB"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");

    let duplicate_dotenv = sandbox_with_deploy_condition("vars.MODE == 'ci'");
    duplicate_dotenv.write(".litci/vars", "Mode=not-ci\nMODE=ci\n");
    let output = duplicate_dotenv.run(&["plan", "-W", MATRIX_NEEDS_PATH, "--json"]);
    assert!(deploy_condition_value(&output));
}

#[test]
fn process_env_resolves_only_referenced_valid_configuration_variables() {
    let sandbox = sandbox_with_deploy_condition("vars.MODE == 'ci'");

    let output = sandbox.run_with_env(
        &["plan", "-W", MATRIX_NEEDS_PATH, "--json"],
        &[
            ("MODE", "ci"),
            ("GITHUB_UNRELATED", "ignored"),
            ("BAD-NAME", "ignored"),
        ],
    );
    assert!(deploy_condition_value(&output));

    let invalid_sandbox = sandbox_with_deploy_condition("vars.GITHUB_MODE == 'ci'");
    let output =
        invalid_sandbox.run_with_env(&["plan", "-W", MATRIX_NEEDS_PATH], &[("GITHUB_MODE", "ci")]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("vars.GITHUB_MODE"), "{stderr}");
    assert!(
        stderr.contains("not a valid GitHub configuration variable name"),
        "{stderr}"
    );
    assert!(stderr.contains("fix:"), "{stderr}");

    // Configuration-variable references are case-insensitive even on a
    // Linux host, whose process environment is case-sensitive.
    let case_name = "GREENLIT_CASE_PROBE_7391";
    let case_sandbox = sandbox_with_deploy_condition(&format!("vars.{case_name} == 'ci'"));
    let lowercase = case_name.to_ascii_lowercase();
    let output = case_sandbox.run_with_env(
        &["plan", "-W", MATRIX_NEEDS_PATH, "--json"],
        &[(lowercase.as_str(), "ci")],
    );
    assert!(deploy_condition_value(&output));

    let output = case_sandbox.run_with_env(
        &["plan", "-W", MATRIX_NEEDS_PATH],
        &[(case_name, "ci"), (lowercase.as_str(), "not-ci")],
    );
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("more than once"), "{stderr}");
    assert!(stderr.contains("--var"), "{stderr}");

    let output = case_sandbox.run_with_os_env(
        &["plan", "-W", MATRIX_NEEDS_PATH],
        &[(
            OsString::from(case_name),
            OsString::from_vec(vec![b'c', b'i', 0xff]),
        )],
    );
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("not valid UTF-8"), "{stderr}");
    assert!(stderr.contains("--var"), "{stderr}");

    let output = case_sandbox.run_with_os_env(
        &[
            "plan",
            "-W",
            MATRIX_NEEDS_PATH,
            "--json",
            "--var",
            &format!("{case_name}=ci"),
        ],
        &[(OsString::from(case_name), OsString::from_vec(vec![0xff]))],
    );
    assert!(deploy_condition_value(&output));
}

#[test]
fn cli_override_wins_over_process_env_and_dotenv() {
    let sandbox = sandbox_with_deploy_condition("vars.MODE == 'ci'");
    sandbox.write(".litci/vars", "MODE=ci\n");

    let output = sandbox.run_with_env(
        &[
            "plan",
            "-W",
            MATRIX_NEEDS_PATH,
            "--json",
            "--var",
            "MODE=not-ci",
        ],
        &[("MODE", "ci")],
    );
    assert!(!deploy_condition_value(&output));
}

#[test]
fn process_env_wins_over_dotenv() {
    let sandbox = sandbox_with_deploy_condition("vars.MODE == 'ci'");
    sandbox.write(".litci/vars", "MODE=ci\n");

    let output = sandbox.run_with_env(
        &["plan", "-W", MATRIX_NEEDS_PATH, "--json"],
        &[("MODE", "not-ci")],
    );
    assert!(!deploy_condition_value(&output));
}

#[test]
fn dynamic_vars_lookup_requires_an_explicit_complete_local_map() {
    let sandbox = sandbox_with_deploy_condition("vars[github.event_name] == 'yes'");

    let output = sandbox.run(&["plan", "-W", MATRIX_NEEDS_PATH]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("dynamic `vars[...]`"));
    assert!(stderr.contains("complete local variable map"));
    assert!(stderr.contains("fixtures/matrix-needs.yml:"));
    assert!(stderr.contains("--var"));
    assert!(stderr.contains(".litci/vars"));
}

#[test]
fn an_existing_empty_dotenv_file_declares_a_complete_empty_map() {
    let sandbox = sandbox_with_deploy_condition("vars[github.event_name] == 'yes'");
    sandbox.write(".litci/vars", "");

    let output = sandbox.run(&["plan", "-W", MATRIX_NEEDS_PATH, "--json"]);
    assert!(!deploy_condition_value(&output));
}

#[test]
fn dynamic_vars_lookup_resolves_when_the_supplied_map_covers_it() {
    // The synthetic push event's `github.event_name` is `"push"`, so
    // supplying `--var push=yes` (even though the workflow never spells
    // `vars.push` literally) is what "a complete locally supplied map"
    // means in this no-auth phase.
    let sandbox = sandbox_with_deploy_condition("vars[github.event_name] == 'yes'");

    let output = sandbox.run(&[
        "plan",
        "-W",
        MATRIX_NEEDS_PATH,
        "--json",
        "--var",
        "push=yes",
    ]);
    assert!(deploy_condition_value(&output));
}

#[test]
fn unknown_context_property_is_falsey_in_the_matrix_needs_plan() {
    // The expression oracle pins the underlying missing-property value as
    // Null and its text conversion as empty. This named-fixture integration
    // case pins the observable workflow-condition consequence.
    let sandbox = sandbox_with_deploy_condition("github.missing");
    let output = sandbox.run(&["plan", "-W", MATRIX_NEEDS_PATH, "--json"]);
    assert!(!deploy_condition_value(&output));
}
