//! Actionable repository states and the default synthetic push context.

use std::process::{Command, Output};

use super::support;
use super::support::Sandbox;

const PUSH_WORKFLOW: &str =
    "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo push\n";

fn git_stdout(sandbox: &Sandbox, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(sandbox.root())
        .output()
        .expect("spawn test git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

fn assert_git_state_failure(name: &str, output: &Output, expected_state: &str, expected_fix: &str) {
    assert!(
        !output.status.success(),
        "row '{name}' unexpectedly planned"
    );
    let stderr = support::stderr_text(output);
    assert!(stderr.contains(expected_state), "row '{name}': {stderr}");
    assert!(stderr.contains(expected_fix), "row '{name}': {stderr}");
}

#[test]
fn repository_state_failures_name_the_single_corrective_action() {
    let non_repository = Sandbox::new();
    non_repository.write("push.yml", PUSH_WORKFLOW);
    let non_repository_output = non_repository.run(&["plan", "-W", "push.yml"]);

    let unborn = Sandbox::new();
    unborn.write("push.yml", PUSH_WORKFLOW);
    unborn.git(&["init", "-q", "-b", "main"]);
    let unborn_output = unborn.run(&["plan", "-W", "push.yml"]);

    let detached = Sandbox::new();
    detached.write("push.yml", PUSH_WORKFLOW);
    detached.init_git();
    detached.git(&["checkout", "-q", "--detach", "HEAD"]);
    let detached_output = detached.run(&["plan", "-W", "push.yml"]);

    for (name, output, state, fix) in [
        (
            "non-repository",
            &non_repository_output,
            "not a git repository (needed to build a synthetic event)",
            "fix: run litci inside a git repository",
        ),
        (
            "unborn repository",
            &unborn_output,
            "git repository has no commits yet (HEAD is unborn)",
            "fix: make an initial commit so HEAD exists",
        ),
        (
            "detached HEAD",
            &detached_output,
            "HEAD is detached (not on a branch)",
            "fix: check out a branch before planning a push event",
        ),
    ] {
        assert_git_state_failure(name, output, state, fix);
    }
}

#[test]
fn default_push_context_uses_local_repository_branch_sha_and_actor() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "push.yml",
        "on: push\njobs:\n  context:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo repo=${{ github.repository }} branch=${{ github.ref_name }} sha=${{ github.sha }} actor=${{ github.actor }}\n",
    );
    sandbox.init_git_on("phase-one");
    sandbox.git(&["config", "user.name", "Phase One Actor"]);

    let sha = git_stdout(&sandbox, &["rev-parse", "HEAD"]);
    let repository = sandbox
        .root()
        .file_name()
        .expect("sandbox repository name")
        .to_string_lossy();
    let output = sandbox.run(&["plan", "-W", "push.yml", "--json"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let plan: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("plan stdout is JSON");
    assert_eq!(plan["event_name"], "push");
    assert_eq!(
        plan["jobs"][0]["steps"][0]["kind"]["script"]["value"],
        format!("echo repo={repository} branch=phase-one sha={sha} actor=Phase One Actor")
    );
}
