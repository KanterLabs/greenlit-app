//! Documented step forms, identifiers, and field combinations.

use greenlit_workflow::model::step::StepAction;
use greenlit_workflow::model::value::{ScalarOrExpr, YamlScalar};
use greenlit_workflow::{Location, ParseError, parse_workflow};

use super::HEADER;

#[test]
fn job_and_step_identifiers_follow_githubs_identifier_rules() {
    let valid = format!(
        "{HEADER}jobs:\n  _Build-2:\n    runs-on: ubuntu-latest\n    steps:\n      - id: Step_1-x\n        run: echo hi\n"
    );
    parse_workflow("ids.yml", &valid).expect("documented identifiers parse");

    let rows = [
        ("2build", "id", 3, 3, 9),
        ("build.test", "id", 3, 3, 13),
        ("build", "-step", 6, 13, 18),
        ("build", "stép", 6, 13, 17),
    ];
    for (job_id, step_id, line, column, end_column) in rows {
        let source = format!(
            "{HEADER}jobs:\n  {job_id}:\n    runs-on: ubuntu-latest\n    steps:\n      - id: {step_id}\n        run: echo hi\n"
        );
        match parse_workflow("ids.yml", &source) {
            Err(ParseError::Schema { span, message }) => {
                assert_eq!(span.start, Location::new(line, column));
                assert_eq!(span.end, Location::new(line, end_column));
                assert!(message.contains("must start with a letter"), "{message}");
            }
            result => panic!("invalid identifier must fail: {result:?}"),
        }
    }

    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - id: Compile\n        run: echo first\n      - id: compile\n        run: echo second\n"
    );
    match parse_workflow("ids.yml", &source) {
        Err(ParseError::Schema { span, message }) => {
            assert_eq!(span.start, Location::new(8, 13));
            assert_eq!(span.end, Location::new(8, 20));
            assert!(message.contains("case-insensitive; rename it"));
            assert!(message.contains("first declared at ids.yml:6:13"));
        }
        result => panic!("case-colliding step ids must fail: {result:?}"),
    }
}

#[test]
fn step_fields_run_form() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - id: s1\n        if: ${{{{ success() }}}}\n        name: Say hi\n        run: echo hi\n        shell: bash\n        env:\n          X: \"1\"\n        working-directory: sub\n        continue-on-error: true\n        timeout-minutes: 5\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    let step = &workflow.jobs[0].steps[0];
    assert_eq!(step.id.as_ref().unwrap().value, "s1");
    assert_eq!(
        step.if_condition.as_ref().unwrap().value,
        "${{ success() }}"
    );
    assert_eq!(
        step.continue_on_error.as_ref().unwrap().value,
        ScalarOrExpr::Literal(YamlScalar::Bool(true))
    );
    assert_eq!(
        step.timeout_minutes.as_ref().unwrap().value,
        ScalarOrExpr::Literal(YamlScalar::Number(5.0))
    );
    match &step.action {
        StepAction::Run { script, shell } => {
            assert_eq!(script.value, "echo hi");
            assert_eq!(shell.as_ref().unwrap().value, "bash");
        }
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn step_fields_uses_form() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          token: ${{{{ secrets.GITHUB_TOKEN }}}}\n          path: sub\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.jobs[0].steps[0].action {
        StepAction::Uses { reference, with } => {
            assert_eq!(reference.value, "actions/checkout@v4");
            assert_eq!(
                with[1].1.value,
                ScalarOrExpr::Literal(YamlScalar::String("sub".into()))
            );
        }
        other => panic!("expected Uses, got {other:?}"),
    }
}

#[test]
fn step_must_have_exactly_one_of_run_or_uses() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - name: neither\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(matches!(err, ParseError::Schema { .. }), "got {err:?}");

    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n        uses: actions/checkout@v4\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(matches!(err, ParseError::Schema { .. }), "got {err:?}");
}

#[test]
fn shell_only_valid_on_run_steps_and_with_only_on_uses_steps() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n        shell: bash\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(matches!(err, ParseError::Schema { .. }), "got {err:?}");

    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n        with:\n          x: y\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(matches!(err, ParseError::Schema { .. }), "got {err:?}");
}

#[test]
fn job_must_have_at_least_one_step() {
    let source = format!("{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps: []\n");
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(matches!(err, ParseError::Schema { .. }), "got {err:?}");
}

#[test]
fn workflow_must_have_at_least_one_job() {
    let source = format!("{HEADER}jobs: {{}}\n");
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(matches!(err, ParseError::Schema { .. }), "got {err:?}");
}
