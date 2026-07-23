//! Oracle tables for event-specific trigger schemas and current
//! `schedule`/`workflow_dispatch` declarations.

use greenlit_workflow::model::trigger::{Trigger, WorkflowDispatchInputType};
use greenlit_workflow::model::value::{ScalarOrExpr, YamlScalar, YamlValue};
use greenlit_workflow::{ParseError, parse_workflow};

const TAIL: &str =
    "jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";

#[test]
fn push_and_pull_request_use_event_specific_filter_schemas() {
    // Current workflow syntax gives tag filters only to push and activity
    // `types` only to pull_request/pull_request_target.
    let rejected = [
        ("push types", "on:\n  push:\n    types: [created]\n"),
        (
            "pull_request tags",
            "on:\n  pull_request:\n    tags: [v1]\n",
        ),
        (
            "pull_request tags-ignore",
            "on:\n  pull_request:\n    tags-ignore: [v1]\n",
        ),
        (
            "pull_request_target tags",
            "on:\n  pull_request_target:\n    tags: [v1]\n",
        ),
    ];
    for (name, trigger) in rejected {
        let source = format!("{trigger}{TAIL}");
        match parse_workflow("event-schema.yml", &source) {
            Err(ParseError::UnknownKey { context, .. }) => {
                assert!(context.starts_with("on."), "row {name}: {context}");
            }
            result => panic!("{name} must be rejected: {result:?}"),
        }
    }

    let accepted = [
        "on:\n  push:\n    tags: [v1]\n",
        "on:\n  pull_request:\n    types: [opened]\n",
        "on:\n  pull_request_target:\n    types: [opened]\n",
    ];
    for trigger in accepted {
        parse_workflow("event-schema.yml", &format!("{trigger}{TAIL}"))
            .expect("event-specific key must parse");
    }
}

#[test]
fn positive_filter_lists_with_negations_require_a_positive_pattern() {
    // GitHub documents this rule independently for branch/tag and path
    // positive filters; `*-ignore` is the exclusion-only form.
    let rejected = [
        (
            "push branches",
            "on:\n  push:\n    branches: ['!legacy/**']\n",
        ),
        ("push tags", "on:\n  push:\n    tags: ['!v0.*']\n"),
        (
            "pull request branches",
            "on:\n  pull_request:\n    branches: ['!legacy/**']\n",
        ),
        (
            "pull request paths",
            "on:\n  pull_request:\n    paths: ['!docs/**']\n",
        ),
    ];
    for (name, trigger) in rejected {
        let error = parse_workflow("negative.yml", &format!("{trigger}{TAIL}"))
            .expect_err("negative-only positive filter must fail");
        match error {
            ParseError::Schema { span, message } => {
                assert_eq!(&*span.file, "negative.yml", "row {name}");
                assert!(
                    message.contains("no positive pattern"),
                    "row {name}: {message}"
                );
            }
            other => panic!("{name}: expected Schema, got {other:?}"),
        }
    }

    let accepted = [
        "on:\n  push:\n    branches: [main, '!legacy/**']\n",
        "on:\n  push:\n    tags-ignore: [v0.*]\n",
        "on:\n  pull_request:\n    paths: [src/**, '!src/generated/**']\n",
        "on:\n  pull_request:\n    paths-ignore: [docs/**]\n",
    ];
    for trigger in accepted {
        parse_workflow("negative.yml", &format!("{trigger}{TAIL}"))
            .expect("documented positive/negative combination must parse");
    }
}

#[test]
fn schedule_requires_nonempty_mappings_with_string_cron_and_optional_timezone() {
    // `timezone` is the current IANA-zone field added to each schedule
    // mapping; absent timezone means UTC.
    let rejected = [
        ("bare schedule", "on: schedule\n"),
        ("empty schedule", "on:\n  schedule: []\n"),
        (
            "missing cron",
            "on:\n  schedule:\n    - timezone: America/New_York\n",
        ),
        (
            "unknown key",
            "on:\n  schedule:\n    - cron: '0 3 * * *'\n      zone: UTC\n",
        ),
        ("numeric cron", "on:\n  schedule:\n    - cron: 5\n"),
        (
            "boolean timezone",
            "on:\n  schedule:\n    - cron: '0 3 * * *'\n      timezone: true\n",
        ),
    ];
    for (name, trigger) in rejected {
        let result = parse_workflow("schedule.yml", &format!("{trigger}{TAIL}"));
        assert!(result.is_err(), "{name} must fail");
    }

    let source = format!(
        "on:\n  schedule:\n    - cron: '30 5 * * 1-5'\n      timezone: America/New_York\n    - cron: '0 12 * * *'\n{TAIL}"
    );
    let workflow = parse_workflow("schedule.yml", &source).expect("schedule parses");
    match &workflow.on[0].value {
        Trigger::Schedule(entries) => {
            assert_eq!(entries[0].cron.value, "30 5 * * 1-5");
            assert_eq!(
                entries[0]
                    .timezone
                    .as_ref()
                    .map(|timezone| timezone.value.as_str()),
                Some("America/New_York")
            );
            assert!(entries[1].timezone.is_none());
        }
        other => panic!("expected Schedule, got {other:?}"),
    }
}

#[test]
fn workflow_dispatch_models_all_types_and_type_checked_defaults() {
    let source = format!(
        "on:\n  workflow_dispatch:\n    inputs:\n      implicit_string:\n        default: hello\n      explicit_string:\n        type: string\n        default: world\n      boolean_value:\n        type: boolean\n        default: false\n      number_value:\n        type: number\n        default: 2.5\n      choice_value:\n        description: Pick one\n        required: true\n        type: choice\n        options: [one, two]\n        default: one\n      environment_value:\n        type: environment\n        default: staging\n{TAIL}"
    );
    let workflow = parse_workflow("dispatch.yml", &source).expect("dispatch parses");
    let inputs = match &workflow.on[0].value {
        Trigger::WorkflowDispatch(dispatch) => &dispatch.inputs,
        other => panic!("expected WorkflowDispatch, got {other:?}"),
    };
    assert_eq!(inputs.len(), 6);
    assert!(inputs[0].input_type.is_none());
    assert_eq!(
        inputs[2].input_type.as_ref().map(|kind| kind.value),
        Some(WorkflowDispatchInputType::Boolean)
    );
    assert_eq!(
        inputs[2].default.as_ref().map(|default| &default.value),
        Some(&YamlValue::Scalar(ScalarOrExpr::Literal(YamlScalar::Bool(
            false
        ))))
    );
    let choice = &inputs[4];
    assert_eq!(choice.name.value, "choice_value");
    assert_eq!(
        choice
            .description
            .as_ref()
            .map(|value| value.value.as_str()),
        Some("Pick one")
    );
    assert_eq!(
        choice.required.as_ref().map(|value| value.value),
        Some(true)
    );
    assert_eq!(
        choice
            .options
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
}

#[test]
fn workflow_dispatch_rejects_invalid_declarations_types_defaults_and_options() {
    let rejected = [
        (
            "invalid identifier",
            "on:\n  workflow_dispatch:\n    inputs:\n      1bad:\n        type: string\n",
        ),
        (
            "scalar declaration",
            "on:\n  workflow_dispatch:\n    inputs:\n      name: text\n",
        ),
        (
            "non-string description",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        description: true\n",
        ),
        (
            "non-boolean required",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        required: yes\n",
        ),
        (
            "unknown type",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: array\n",
        ),
        (
            "boolean string default",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: boolean\n        default: 'false'\n",
        ),
        (
            "number string default",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: number\n        default: '2'\n",
        ),
        (
            "string boolean default",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: string\n        default: false\n",
        ),
        (
            "environment number default",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: environment\n        default: 2\n",
        ),
        (
            "structured default",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: string\n        default: [one]\n",
        ),
        (
            "choice without options",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: choice\n",
        ),
        (
            "options on string",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: string\n        options: [one]\n",
        ),
        (
            "scalar options",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: choice\n        options: one\n",
        ),
        (
            "non-string option",
            "on:\n  workflow_dispatch:\n    inputs:\n      name:\n        type: choice\n        options: [one, 2]\n",
        ),
    ];
    for (name, trigger) in rejected {
        let result = parse_workflow("dispatch.yml", &format!("{trigger}{TAIL}"));
        assert!(result.is_err(), "{name} must fail");
    }
}

#[test]
fn workflow_dispatch_enforces_the_current_twenty_five_input_limit() {
    let declarations = (0..25)
        .map(|index| format!("      input_{index}:\n        type: string\n"))
        .collect::<String>();
    let source = format!("on:\n  workflow_dispatch:\n    inputs:\n{declarations}{TAIL}");
    parse_workflow("dispatch.yml", &source).expect("25 inputs are allowed");

    let source = format!(
        "on:\n  workflow_dispatch:\n    inputs:\n{declarations}      input_25:\n        type: string\n{TAIL}"
    );
    match parse_workflow("dispatch.yml", &source) {
        Err(ParseError::Schema { message, .. }) => {
            assert!(message.contains("maximum of 25"), "{message}");
        }
        result => panic!("26 inputs must fail, got {result:?}"),
    }
}
