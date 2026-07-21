//! Oracle table: recognized-but-rejected constructs
//! (`greenlit-v0-spec.md` "Out (v0)": `concurrency`, environments/
//! deployments, reusable workflows). Parsing must succeed and the
//! construct's location must be preserved, per `PHASE-1-engine-core.md`:
//! "parsing succeeds, planning fails with a precise 'not in v0' message
//! naming the construct and its location."

use greenlit_workflow::parse_workflow;

const HEADER: &str = "on: push\n";

#[test]
fn workflow_level_concurrency_is_recognized_but_marked_unsupported() {
    let source = format!(
        "{HEADER}concurrency:\n  group: ${{{{ github.ref }}}}\n  cancel-in-progress: true\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("must still parse");
    let concurrency = workflow.concurrency.expect("concurrency must be recorded");
    assert_eq!(concurrency.name, "concurrency");
    assert_eq!(concurrency.location.start.line, 2);
}

#[test]
fn job_level_concurrency_is_recognized_but_marked_unsupported() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    concurrency: build-group\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("must still parse");
    let concurrency = workflow.jobs[0]
        .concurrency
        .clone()
        .expect("concurrency must be recorded");
    assert_eq!(concurrency.name, "concurrency");
}

#[test]
fn job_level_environment_is_recognized_but_marked_unsupported() {
    let source = format!(
        "{HEADER}jobs:\n  deploy:\n    runs-on: ubuntu-latest\n    environment:\n      name: production\n      url: https://example.com\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("must still parse");
    let environment = workflow.jobs[0]
        .environment
        .clone()
        .expect("environment must be recorded");
    assert_eq!(environment.name, "environment");
}

#[test]
fn reusable_workflow_call_job_is_recognized_but_marked_unsupported() {
    let source = format!(
        "{HEADER}jobs:\n  call-shared:\n    needs: []\n    uses: my-org/shared/.github/workflows/build.yml@main\n    with:\n      version: 1\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("must still parse");
    let job = &workflow.jobs[0];
    let reusable = job
        .reusable_call
        .clone()
        .expect("reusable_call must be recorded");
    assert!(reusable.name.contains("reusable workflow"));
    assert!(job.runs_on.is_none());
    assert!(job.steps.is_empty());
}
