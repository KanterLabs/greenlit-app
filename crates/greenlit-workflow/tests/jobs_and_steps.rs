//! Oracle table: documented `jobs.<id>:` and `jobs.<id>.steps[]:` fields,
//! including identifier, runner, dependency, environment, and action rules.

use greenlit_workflow::model::job::RunsOn;
use greenlit_workflow::model::step::StepAction;
use greenlit_workflow::model::value::{ScalarOrExpr, YamlScalar};
use greenlit_workflow::model::workflow::{PermissionLevel, PermissionLevelAll, Permissions};
use greenlit_workflow::{Location, ParseError, extract_static, parse_workflow};

const HEADER: &str = "on: push\n";

#[test]
fn static_extraction_reports_the_complete_preflight_inventory() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    env:\n      TOKEN: ${{{{ secrets.API_TOKEN }}}}\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          region: ${{{{ vars.REGION }}}}\n      - run: echo ${{{{ secrets['DB_PASSWORD'] }}}} ${{{{ vars['DEPLOY_ENV'] }}}} ${{{{ vars[matrix.key] }}}}\n  package:\n    runs-on: [self-hosted, linux]\n    steps:\n      - uses: actions/setup-node@v4\n"
    );
    let workflow = parse_workflow("inventory.yml", &source).expect("parses");
    let extraction = extract_static(&workflow).expect("valid expressions extract");

    let secret_names: Vec<&str> = extraction.secrets.keys().map(String::as_str).collect();
    assert_eq!(secret_names, ["API_TOKEN", "DB_PASSWORD"]);
    let variable_names: Vec<&str> = extraction.vars.keys().map(String::as_str).collect();
    assert_eq!(variable_names, ["DEPLOY_ENV", "REGION"]);
    assert!(extraction.has_dynamic_vars_lookup);
    assert_eq!(extraction.dynamic_vars.len(), 1);

    let uses: Vec<&str> = extraction
        .uses
        .iter()
        .map(|reference| reference.value.as_str())
        .collect();
    assert_eq!(uses, ["actions/checkout@v4", "actions/setup-node@v4"]);
    let runner_labels: Vec<&str> = extraction
        .runs_on
        .iter()
        .map(|label| label.value.as_str())
        .collect();
    assert_eq!(runner_labels, ["ubuntu-latest", "self-hosted", "linux"]);

    let literal_only = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{{{ vars.REGION }}}}\n"
    );
    let workflow = parse_workflow("literal-only.yml", &literal_only).expect("parses");
    let extraction = extract_static(&workflow).expect("valid expressions extract");
    assert!(!extraction.has_dynamic_vars_lookup);
}

#[test]
fn runs_on_single_label() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.jobs[0].runs_on.as_ref().unwrap().value {
        RunsOn::Label(label) => assert_eq!(label.value, "ubuntu-latest"),
        other => panic!("expected Label, got {other:?}"),
    }
}

#[test]
fn runs_on_expression_is_preserved_raw() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ${{{{ matrix.os }}}}\n    strategy:\n      matrix:\n        os: [ubuntu-latest]\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.jobs[0].runs_on.as_ref().unwrap().value {
        RunsOn::Label(label) => assert_eq!(label.value, "${{ matrix.os }}"),
        other => panic!("expected Label, got {other:?}"),
    }
}

#[test]
fn runs_on_label_list() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: [self-hosted, linux, x64]\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.jobs[0].runs_on.as_ref().unwrap().value {
        RunsOn::Labels(labels) => {
            let texts: Vec<&str> = labels.iter().map(|l| l.value.as_str()).collect();
            assert_eq!(texts, ["self-hosted", "linux", "x64"]);
        }
        other => panic!("expected Labels, got {other:?}"),
    }
}

#[test]
fn runs_on_group_form() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on:\n      group: my-group\n      labels: [linux]\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.jobs[0].runs_on.as_ref().unwrap().value {
        RunsOn::Group { group, labels } => {
            assert_eq!(group.as_ref().unwrap().value, "my-group");
            assert_eq!(labels[0].value, "linux");
        }
        other => panic!("expected Group, got {other:?}"),
    }
}

#[test]
fn missing_runs_on_is_a_missing_key_error() {
    let source = format!("{HEADER}jobs:\n  build:\n    steps:\n      - run: echo hi\n");
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(matches!(err, ParseError::MissingKey { key: "runs-on", .. }));
}

#[test]
fn needs_normalizes_single_string_and_list() {
    let source = format!(
        "{HEADER}jobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n  b:\n    needs: a\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n  c:\n    needs: [a, b]\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    assert_eq!(workflow.jobs[1].needs[0].value, "a");
    let c_needs: Vec<&str> = workflow.jobs[2]
        .needs
        .iter()
        .map(|n| n.value.as_str())
        .collect();
    assert_eq!(c_needs, ["a", "b"]);
}

#[test]
fn job_outputs_are_retained_as_raw_expression_text() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    outputs:\n      sha: ${{{{ steps.x.outputs.sha }}}}\n    steps:\n      - id: x\n        run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    assert_eq!(workflow.jobs[0].outputs[0].0.value, "sha");
    assert_eq!(
        workflow.jobs[0].outputs[0].1.value,
        "${{ steps.x.outputs.sha }}"
    );
}

#[test]
fn job_env_and_defaults() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    env:\n      FOO: bar\n    defaults:\n      run:\n        shell: bash\n        working-directory: sub\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    let job = &workflow.jobs[0];
    assert_eq!(job.env[0].0.value, "FOO");
    assert_eq!(
        job.env[0].1.value,
        ScalarOrExpr::Literal(YamlScalar::String("bar".into()))
    );
    let defaults = job.defaults.as_ref().unwrap();
    let run = defaults.value.run.as_ref().unwrap();
    assert_eq!(run.value.shell.as_ref().unwrap().value, "bash");
}

#[test]
fn workflow_level_permissions_all_and_scoped() {
    let source = format!(
        "name: x\n{HEADER}permissions: read-all\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.permissions.as_ref().unwrap().value {
        Permissions::All(PermissionLevelAll::ReadAll) => {}
        other => panic!("expected All(ReadAll), got {other:?}"),
    }

    let source = format!(
        "{HEADER}permissions:\n  contents: read\n  pull-requests: write\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.permissions.as_ref().unwrap().value {
        Permissions::Scoped(scopes) => {
            assert_eq!(scopes[0].0.value, "contents");
            assert_eq!(scopes[0].1.value, PermissionLevel::Read);
            assert_eq!(scopes[1].1.value, PermissionLevel::Write);
        }
        other => panic!("expected Scoped, got {other:?}"),
    }
}

#[test]
fn permission_scope_names_use_githubs_closed_set() {
    // Current scope list from GitHub's "Defining access for the
    // GITHUB_TOKEN scopes" table:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#defining-access-for-the-github_token-scopes
    let scopes = [
        "actions",
        "artifact-metadata",
        "attestations",
        "checks",
        "code-quality",
        "contents",
        "deployments",
        "discussions",
        "id-token",
        "issues",
        "models",
        "packages",
        "pages",
        "pull-requests",
        "security-events",
        "statuses",
        "vulnerability-alerts",
    ];
    let permission_rows = scopes
        .iter()
        .map(|scope| format!("  {scope}: none\n"))
        .collect::<String>();
    let source = format!(
        "{HEADER}permissions:\n{permission_rows}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("all documented scopes parse");
    match &workflow.permissions.as_ref().expect("permissions").value {
        Permissions::Scoped(parsed) => assert_eq!(parsed.len(), scopes.len()),
        other => panic!("expected Scoped, got {other:?}"),
    }

    let source = format!(
        "{HEADER}permissions:\n  contnets: read\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("unknown scope must fail");
    match err {
        ParseError::UnknownKey { span, key, context } => {
            assert_eq!(key, "contnets");
            assert_eq!(context, "permissions");
            assert_eq!(span.start.line, 3);
            assert_eq!(span.start.column, 3);
        }
        other => panic!("expected UnknownKey, got {other:?}"),
    }
}

#[test]
fn services_and_container_share_the_same_shape() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    container:\n      image: node:20\n      env:\n        NODE_ENV: test\n    services:\n      redis:\n        image: redis:7\n        ports: [\"6379:6379\"]\n        credentials:\n          username: u\n          password: ${{{{ secrets.REDIS_PASS }}}}\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    let job = &workflow.jobs[0];
    let container = &job.container.as_ref().unwrap().value;
    assert_eq!(
        container.image.value,
        ScalarOrExpr::Literal(YamlScalar::String("node:20".into()))
    );
    let (svc_name, svc) = &job.services[0];
    assert_eq!(svc_name.value, "redis");
    assert_eq!(
        svc.value.ports[0].value,
        ScalarOrExpr::Literal(YamlScalar::String("6379:6379".into()))
    );
    let creds = svc.value.credentials.as_ref().unwrap();
    assert!(matches!(
        creds.value.password.as_ref().unwrap().value,
        ScalarOrExpr::Expression(_)
    ));
}

#[test]
fn container_bare_string_shorthand() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    container: node:20-slim\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    let container = &workflow.jobs[0].container.as_ref().unwrap().value;
    assert_eq!(
        container.image.value,
        ScalarOrExpr::Literal(YamlScalar::String("node:20-slim".into()))
    );
    assert!(container.credentials.is_none());
}

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
