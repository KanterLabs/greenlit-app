//! Synthetic pull-request event contract.

use super::common::*;

#[test]
fn pull_request_event_exposes_the_documented_synthetic_shape() {
    let sandbox = sandbox_with_workflow(RICH_WORKFLOW);
    let (plan, _, _) = plan_json(&sandbox, &["-e", "pull_request"]);

    assert_eq!(plan["event_name"], "pull_request");
    assert_eq!(job(&plan, "pr_shape")["condition"]["evaluation"], "static");
    assert_eq!(job(&plan, "pr_shape")["condition"]["value"], true);

    // GitHub uses event-specific run information when `run-name` is absent
    // or resolves to whitespace.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#run-name
    let fallback = sandbox_with_workflow(
        "run-name: '   '\non: pull_request\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
    );
    let (fallback_plan, _, _) = plan_json(&fallback, &["-e", "pull_request"]);
    assert!(fallback_plan["run_name"].is_null());

    // GitHub assigns `github.sha` to the PR merge commit, not the topic
    // branch's HEAD. Planning has no merge object yet, so both that identity
    // and the commit supplying the workflow stay explicit runtime slots.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#pull_request
    let sha_workflow = sandbox_with_workflow(
        "on: pull_request\njobs:\n  sha:\n    runs-on: ubuntu-latest\n    if: github.sha[fromJSON('bad')] == '' || github.workflow_sha == github.sha\n    steps:\n      - run: echo sha\n",
    );
    let (sha_plan, _, _) = plan_json(&sha_workflow, &["-e", "pull_request"]);
    let condition = &job(&sha_plan, "sha")["condition"];
    assert_eq!(condition["evaluation"], "deferred");
    let properties = condition["defers_on"]
        .as_array()
        .expect("PR SHA dependencies")
        .iter()
        .filter(|reason| reason["kind"] == "github-context")
        .map(|reason| reason["property"].as_str().expect("specific property"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        properties,
        std::collections::BTreeSet::from(["sha", "workflow_sha"])
    );

    // A locally edited workflow was not supplied by HEAD, so assigning
    // `github.workflow_sha` to HEAD would invent provenance. The event SHA
    // remains known for a push, while only workflow provenance defers.
    let dirty = sandbox_with_workflow(
        "on: push\njobs:\n  provenance:\n    runs-on: ubuntu-latest\n    if: github.workflow_sha == github.sha\n    steps:\n      - run: echo provenance\n",
    );
    dirty.write(
        "contracts.yml",
        "on: push\njobs:\n  provenance:\n    runs-on: ubuntu-latest\n    if: github.workflow_sha == github.sha\n    steps:\n      - run: echo provenance\n# local edit\n",
    );
    let (dirty_plan, _, _) = plan_json(&dirty, &[]);
    let condition = &job(&dirty_plan, "provenance")["condition"];
    assert_eq!(condition["evaluation"], "deferred");
    assert_eq!(
        condition["defers_on"],
        serde_json::json!([{
            "kind": "github-context",
            "property": "workflow_sha"
        }])
    );
}
