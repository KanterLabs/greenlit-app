//! Oracle table: source spans (file, line, column) are preserved on model
//! nodes (`PHASE-1-engine-core.md`: "Preserve source spans (file, line,
//! column) on every node for error messages"), and use GitHub's own
//! 1-based line/column convention (design memo §6.1).

use greenlit_workflow::model::job::RunsOn;
use greenlit_workflow::model::step::StepAction;
use greenlit_workflow::model::trigger::Trigger;
use greenlit_workflow::parse_workflow;

const SOURCE: &str =
    "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";

#[test]
fn every_span_carries_the_file_name_given_to_parse_workflow() {
    let workflow = parse_workflow("ci.yml", SOURCE).expect("parses");
    assert_eq!(&*workflow.span.file, "ci.yml");
    assert_eq!(&*workflow.jobs[0].id.span.file, "ci.yml");
}

#[test]
fn job_id_key_span_points_at_the_key_text() {
    let workflow = parse_workflow("ci.yml", SOURCE).expect("parses");
    let id = &workflow.jobs[0].id;
    // "  build:" — two leading spaces, `build` starts at column 3.
    assert_eq!(id.span.start.line, 3);
    assert_eq!(id.span.start.column, 3);
    assert_eq!(id.span.end.line, 3);
    assert_eq!(id.span.end.column, 8); // exclusive end, just past the 'd'.
}

#[test]
fn runs_on_value_span_points_at_the_scalar_not_the_key() {
    let workflow = parse_workflow("ci.yml", SOURCE).expect("parses");
    let runs_on = workflow.jobs[0].runs_on.as_ref().expect("runs-on present");
    match &runs_on.value {
        RunsOn::Label(label) => {
            // "    runs-on: ubuntu-latest" — value starts at column 14.
            assert_eq!(label.span.start.line, 4);
            assert_eq!(label.span.start.column, 14);
        }
        other => panic!("expected Label, got {other:?}"),
    }
}

#[test]
fn step_run_script_span() {
    let workflow = parse_workflow("ci.yml", SOURCE).expect("parses");
    match &workflow.jobs[0].steps[0].action {
        StepAction::Run { script, .. } => {
            // "      - run: echo hi" — script starts at column 14.
            assert_eq!(script.span.start.line, 6);
            assert_eq!(script.span.start.column, 14);
        }
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn whole_document_span_starts_at_line_one_column_one() {
    let workflow = parse_workflow("ci.yml", SOURCE).expect("parses");
    assert_eq!(workflow.span.start.line, 1);
    assert_eq!(workflow.span.start.column, 1);
}

#[test]
fn jobs_and_mapped_trigger_nodes_keep_their_complete_value_spans() {
    // PHASE-1-engine-core.md requires the node's own source span, rather
    // than a neighboring mapping-key span, on every typed node.
    let source = "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n    inputs:\n      target:\n        description: Deploy target\n        type: string\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");

    let job = &workflow.jobs[0];
    assert_ne!(job.span, job.id.span);
    assert!(job.span.start <= job.runs_on.as_ref().expect("runs-on").span.start);
    assert!(job.span.end >= job.steps[0].span.end);

    match &workflow.on[0].value {
        Trigger::Webhook { filter, .. } => {
            assert_eq!(workflow.on[0].span, filter.span);
            assert!(filter.span.start <= filter.branches[0].span.start);
            assert!(filter.span.end >= filter.branches[0].span.end);
        }
        other => panic!("expected Webhook, got {other:?}"),
    }

    match &workflow.on[1].value {
        Trigger::WorkflowDispatch(dispatch) => {
            let input = &dispatch.inputs[0];
            assert!(workflow.on[1].span.start <= input.span.start);
            assert!(workflow.on[1].span.end >= input.span.end);
            assert_ne!(input.span, input.name.span);
            assert!(
                input.span.start <= input.description.as_ref().expect("description").span.start
            );
            assert!(input.span.end >= input.input_type.as_ref().expect("type").span.end);
        }
        other => panic!("expected WorkflowDispatch, got {other:?}"),
    }
}
