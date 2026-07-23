//! Repository-rooted explicit and implicit workflow discovery.

use super::common::*;
use super::support;
use super::support::Sandbox;

#[test]
fn readable_non_utf8_workflow_requests_encoding_repair_not_permissions() {
    let sandbox = Sandbox::new();
    sandbox.write_bytes(
        "invalid-encoding.yml",
        b"on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo \xff\n",
    );
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", "invalid-encoding.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("workflow file is not valid UTF-8"),
        "{stderr}"
    );
    assert!(
        stderr.contains("fix: save the workflow file as valid UTF-8, then retry"),
        "{stderr}"
    );
    assert!(!stderr.contains("check the path"), "{stderr}");
    assert!(!stderr.contains("file permissions"), "{stderr}");
}

#[test]
fn discovery_explicit_paths_and_dotenv_are_rooted_at_the_repository_from_a_subdirectory() {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/ci.yml", SUBDIRECTORY_WORKFLOW);
    sandbox.write(".litci/vars", "mode=ci\n");
    sandbox.write("packages/api/.keep", "");
    sandbox.init_git();

    for args in [
        vec!["plan"],
        vec!["plan", "-W", "../../.github/workflows/ci.yml"],
    ] {
        let output = sandbox.run_from("packages/api", &args);
        assert!(output.status.success(), "{}", support::stderr_text(&output));
        assert!(condition_line(&support::stdout_text(&output)).contains("static(true)"));
    }
}

#[test]
fn missing_workflows_directory_fails_with_a_fix_naming_dash_w() {
    let sandbox = Sandbox::new();
    sandbox.init_git();

    for (args, expected) in [
        (vec!["plan"], ".github/workflows"),
        (
            vec!["plan", "-W", "missing.yml"],
            "could not resolve workflow file",
        ),
    ] {
        let output = sandbox.run(&args);
        assert!(!output.status.success());
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains("-W"), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
        assert!(stderr.contains("fix:"), "{stderr}");
    }
}

#[test]
fn ambiguous_workflows_directory_fails_and_lists_the_candidates() {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/a.yml", LITERAL_VAR_WORKFLOW);
    sandbox.write(".github/workflows/b.yml", LITERAL_VAR_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run(&["plan"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("a.yml"));
    assert!(stderr.contains("b.yml"));
    assert!(stderr.contains("-W"));

    let many = Sandbox::new();
    for index in 0..25 {
        many.write(
            &format!(".github/workflows/workflow-{index:02}.yml"),
            LITERAL_VAR_WORKFLOW,
        );
    }
    many.init_git();
    let output = many.run(&["plan"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("additional candidates omitted"), "{stderr}");
    assert!(
        stderr.len() < 8 * 1024,
        "ambiguous diagnostic was unbounded"
    );
}
