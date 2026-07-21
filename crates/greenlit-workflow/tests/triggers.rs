//! Oracle table: `on:` — all trigger forms
//! (`PHASE-1-engine-core.md` greenlit-workflow section).

use greenlit_workflow::model::trigger::Trigger;
use greenlit_workflow::model::value::UnsupportedConstruct;
use greenlit_workflow::{ParseError, parse_workflow};

const TAIL: &str =
    "jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";

#[test]
fn bare_scalar_form() {
    let source = format!("on: push\n{TAIL}");
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    assert_eq!(workflow.on.len(), 1);
    match &workflow.on[0].value {
        Trigger::Webhook { name, filter } => {
            assert_eq!(name, "push");
            assert!(filter.branches.is_empty());
        }
        other => panic!("expected Webhook, got {other:?}"),
    }
}

#[test]
fn sequence_form() {
    let source = format!("on: [push, pull_request]\n{TAIL}");
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    assert_eq!(workflow.on.len(), 2);
    let names: Vec<&str> = workflow
        .on
        .iter()
        .map(|t| match &t.value {
            Trigger::Webhook { name, .. } => name.as_str(),
            other => panic!("expected Webhook, got {other:?}"),
        })
        .collect();
    assert_eq!(names, ["push", "pull_request"]);
}

#[test]
fn mapping_form_with_filters() {
    let source = format!(
        "on:\n  push:\n    branches: [main, staging]\n    paths-ignore: [\"**.md\"]\n  pull_request:\n    types: [opened, synchronize]\n{TAIL}"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    assert_eq!(workflow.on.len(), 2);
    match &workflow.on[0].value {
        Trigger::Webhook { name, filter } => {
            assert_eq!(name, "push");
            let branches: Vec<&str> = filter.branches.iter().map(|b| b.value.as_str()).collect();
            assert_eq!(branches, ["main", "staging"]);
            assert_eq!(filter.paths_ignore[0].value, "**.md");
        }
        other => panic!("expected Webhook, got {other:?}"),
    }
    match &workflow.on[1].value {
        Trigger::Webhook { name, filter } => {
            assert_eq!(name, "pull_request");
            let types: Vec<&str> = filter.types.iter().map(|t| t.value.as_str()).collect();
            assert_eq!(types, ["opened", "synchronize"]);
        }
        other => panic!("expected Webhook, got {other:?}"),
    }
}

#[test]
fn mapping_key_with_null_value_means_no_config() {
    let source = format!("on:\n  push:\n  pull_request: null\n{TAIL}");
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    for trigger in &workflow.on {
        match &trigger.value {
            Trigger::Webhook { filter, .. } => assert!(filter.branches.is_empty()),
            other => panic!("expected Webhook, got {other:?}"),
        }
    }
}

#[test]
fn workflow_dispatch_with_inputs() {
    let source = format!(
        "on:\n  workflow_dispatch:\n    inputs:\n      environment:\n        description: \"Target env\"\n        required: true\n        type: choice\n        options: [staging, production]\n      debug:\n        type: boolean\n        default: false\n{TAIL}"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.on[0].value {
        Trigger::WorkflowDispatch(dispatch) => {
            assert_eq!(dispatch.inputs.len(), 2);
            let env_input = &dispatch.inputs[0];
            assert_eq!(env_input.name.value, "environment");
            assert_eq!(env_input.required.as_ref().map(|r| r.value), Some(true));
            let options: Vec<&str> = env_input.options.iter().map(|o| o.value.as_str()).collect();
            assert_eq!(options, ["staging", "production"]);
        }
        other => panic!("expected WorkflowDispatch, got {other:?}"),
    }
}

#[test]
fn schedule_with_cron_entries() {
    let source = format!(
        "on:\n  schedule:\n    - cron: \"0 3 * * *\"\n      timezone: America/New_York\n    - cron: \"0 15 * * *\"\n{TAIL}"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.on[0].value {
        Trigger::Schedule(crons) => {
            let texts: Vec<&str> = crons.iter().map(|c| c.cron.value.as_str()).collect();
            assert_eq!(texts, ["0 3 * * *", "0 15 * * *"]);
            assert_eq!(
                crons[0].timezone.as_ref().map(|zone| zone.value.as_str()),
                Some("America/New_York")
            );
            assert!(crons[1].timezone.is_none());
        }
        other => panic!("expected Schedule, got {other:?}"),
    }
}

#[test]
fn repository_dispatch_with_types() {
    let source = format!("on:\n  repository_dispatch:\n    types: [deploy, rollback]\n{TAIL}");
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.on[0].value {
        Trigger::RepositoryDispatch { types } => {
            let texts: Vec<&str> = types.iter().map(|t| t.value.as_str()).collect();
            assert_eq!(texts, ["deploy", "rollback"]);
        }
        other => panic!("expected RepositoryDispatch, got {other:?}"),
    }
}

#[test]
fn workflow_call_is_recognized_but_marked_unsupported() {
    let source = format!("on:\n  workflow_call:\n{TAIL}");
    let workflow = parse_workflow("t.yml", &source).expect("workflow_call must still parse");
    match &workflow.on[0].value {
        Trigger::WorkflowCall(UnsupportedConstruct { name, .. }) => {
            assert_eq!(*name, "workflow_call")
        }
        other => panic!("expected WorkflowCall, got {other:?}"),
    }
}

#[test]
fn unrecognized_event_name_falls_back_to_generic_other() {
    let source = format!("on:\n  release:\n    types: [published]\n{TAIL}");
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.on[0].value {
        Trigger::Other { name, config } => {
            assert_eq!(name, "release");
            assert!(config.is_some());
        }
        other => panic!("expected Other, got {other:?}"),
    }
}

#[test]
fn mutually_exclusive_event_filter_pairs_are_rejected() {
    // GitHub forbids the positive and `-ignore` forms together for branch,
    // tag, and path filters on one event:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushbranchestagsbranches-ignoretags-ignore
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushpull_requestpull_request_targetpathspaths-ignore
    for (positive, ignored) in [
        ("branches", "branches-ignore"),
        ("tags", "tags-ignore"),
        ("paths", "paths-ignore"),
    ] {
        let source =
            format!("on:\n  push:\n    {positive}: [main]\n    {ignored}: [legacy]\n{TAIL}");
        let err = parse_workflow("t.yml", &source).expect_err("filters must conflict");
        match err {
            ParseError::Schema { span, message } => {
                assert_eq!(span.start.line, 4, "pair {positive}/{ignored}");
                assert!(message.contains(positive), "got {message:?}");
                assert!(message.contains(ignored), "got {message:?}");
                assert!(message.contains("t.yml:3:5"), "got {message:?}");
                assert!(message.contains("t.yml:4:5"), "got {message:?}");
            }
            other => panic!("expected Schema for {positive}/{ignored}, got {other:?}"),
        }
    }
}
