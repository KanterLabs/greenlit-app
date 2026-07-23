//! The repo's own dogfood workflow stays plannable by litci.
//!
//! TESTING.md's dogfood clause requires this repository's workflow to run
//! green under Greenlit from Phase 2 onward. The live run needs a container
//! engine, but planability is daemonless: this guard embeds the committed
//! `.github/workflows/dogfood.yml` and pins that it (a) plans cleanly as a
//! shell-only workflow (no `uses:` creep) and (b) declares only
//! `workflow_dispatch`, so GitHub never auto-runs it and a plain `push`
//! event does not select it.

use super::support;
use super::support::Sandbox;

const DOGFOOD_WORKFLOW: &str = include_str!("../../../../.github/workflows/dogfood.yml");

#[test]
fn dogfood_workflow_plans_cleanly_under_workflow_dispatch() {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/dogfood.yml", DOGFOOD_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run(&[
        "plan",
        "-W",
        ".github/workflows/dogfood.yml",
        "-e",
        "workflow_dispatch",
    ]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
}

#[test]
fn dogfood_workflow_is_gated_to_workflow_dispatch_only() {
    let sandbox = Sandbox::new();
    sandbox.write(".github/workflows/dogfood.yml", DOGFOOD_WORKFLOW);
    sandbox.init_git();

    let output = sandbox.run(&["plan", "-W", ".github/workflows/dogfood.yml"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("push"), "{stderr}");
}
