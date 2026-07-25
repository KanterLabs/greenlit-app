//! Graph and recognized-but-out-of-v0 rejection contracts.

use super::common::*;
use super::support;

#[test]
fn graph_failures_name_the_jobs_and_render_the_exact_span_and_fix() {
    let rows = [
        (
            "unknown need",
            "on: push\njobs:\n  consumer:\n    needs: missing\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            [
                "contracts.yml:4:12",
                "job 'consumer' needs unknown job 'missing'",
                "fix: fix the `needs:` entry",
            ],
        ),
        (
            "named cycle",
            "on: push\njobs:\n  alpha:\n    needs: beta\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo alpha\n  beta:\n    needs: alpha\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo beta\n",
            [
                "contracts.yml:3:3",
                "dependency cycle: alpha -> beta -> alpha",
                "fix: break the cycle",
            ],
        ),
    ];

    for (name, source, expected) in rows {
        let sandbox = sandbox_with_workflow(source);
        let output = sandbox.run(&["plan", "-W", "contracts.yml"]);
        assert!(!output.status.success(), "row '{name}' must fail");
        let stderr = support::stderr_text(&output);
        for fragment in expected {
            assert!(stderr.contains(fragment), "row '{name}': {stderr}");
        }
    }
}

#[test]
fn every_remaining_recognized_v0_construct_fails_at_its_authored_key() {
    let rows = [
        (
            "environment",
            "on:\n  pull_request:\n    branches: [main]\njobs:\n  build:\n    environment: production\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            "environment",
            "contracts.yml:6:5",
            Some("does not satisfy its trigger filters"),
        ),
        (
            "reusable call job",
            "on: push\njobs:\n  call:\n    uses: ./.github/workflows/reusable.yml\n    strategy:\n      matrix:\n        version: [20, 22]\n    concurrency:\n      group: reusable-${{ github.ref }}\n      cancel-in-progress: true\n",
            "reusable workflow call (jobs.<id>.uses)",
            "contracts.yml:4:5",
            None,
        ),
    ];

    for (name, source, construct, location, forbidden) in rows {
        let sandbox = sandbox_with_workflow(source);
        let output = sandbox.run(&["plan", "-W", "contracts.yml"]);
        assert!(!output.status.success(), "row '{name}' must fail");
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(location), "row '{name}': {stderr}");
        assert!(
            stderr.contains(&format!("{construct}: not in v0")),
            "row '{name}': {stderr}"
        );
        assert!(
            stderr.contains("fix: remove or restructure the workflow"),
            "row '{name}': {stderr}"
        );
        if let Some(forbidden) = forbidden {
            assert!(!stderr.contains(forbidden), "row '{name}': {stderr}");
        }
    }
}
