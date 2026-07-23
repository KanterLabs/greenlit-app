//! Documented job-level workflow fields and closed schemas.

use greenlit_workflow::model::job::RunsOn;
use greenlit_workflow::model::value::{ScalarOrExpr, YamlScalar};
use greenlit_workflow::model::workflow::{PermissionLevel, PermissionLevelAll, Permissions};
use greenlit_workflow::{ParseError, parse_workflow};

use super::HEADER;

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
fn workflow_and_job_permissions_support_all_and_scoped_forms() {
    let source = format!(
        "name: x\n{HEADER}permissions: read-all\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.permissions.as_ref().unwrap().value {
        Permissions::All(PermissionLevelAll::ReadAll) => {}
        other => panic!("expected All(ReadAll), got {other:?}"),
    }

    let source = format!(
        "{HEADER}permissions:\n  contents: read\n  pull-requests: write\n  id-token: write\n  models: read\n  vulnerability-alerts: read\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    match &workflow.permissions.as_ref().unwrap().value {
        Permissions::Scoped(scopes) => {
            assert_eq!(scopes[0].0.value, "contents");
            assert_eq!(scopes[0].1.value, PermissionLevel::Read);
            assert_eq!(scopes[1].1.value, PermissionLevel::Write);
            assert_eq!(scopes[2].1.value, PermissionLevel::Write);
            assert_eq!(scopes[3].1.value, PermissionLevel::Read);
            assert_eq!(scopes[4].1.value, PermissionLevel::Read);
        }
        other => panic!("expected Scoped, got {other:?}"),
    }

    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    permissions: write-all\n    steps:\n      - run: echo hi\n  test:\n    runs-on: ubuntu-latest\n    permissions:\n      contents: read\n      actions: none\n      id-token: none\n      models: none\n      vulnerability-alerts: none\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("job-level permissions parse");
    assert!(matches!(
        workflow.jobs[0].permissions.as_ref().map(|p| &p.value),
        Some(Permissions::All(PermissionLevelAll::WriteAll))
    ));
    match &workflow.jobs[1]
        .permissions
        .as_ref()
        .expect("permissions")
        .value
    {
        Permissions::Scoped(scopes) => {
            assert_eq!(scopes[0].0.value, "contents");
            assert_eq!(scopes[0].1.value, PermissionLevel::Read);
            assert_eq!(scopes[1].0.value, "actions");
            assert_eq!(scopes[1].1.value, PermissionLevel::None);
            assert_eq!(scopes[2].1.value, PermissionLevel::None);
            assert_eq!(scopes[3].1.value, PermissionLevel::None);
            assert_eq!(scopes[4].1.value, PermissionLevel::None);
        }
        other => panic!("expected job Scoped, got {other:?}"),
    }
}

#[test]
fn reusable_call_recognizes_strategy_and_concurrency_before_v0_rejection() {
    // GitHub includes both keys in the closed caller-job keyword set. The
    // reusable call remains represented as one unsupported v0 construct so
    // the planner, rather than the YAML parser, can issue the promised
    // construct-level diagnostic.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/reusing-workflow-configurations#supported-keywords-for-jobs-that-call-a-reusable-workflow
    let source = "on: push\njobs:\n  call:\n    uses: ./.github/workflows/reusable.yml\n    strategy:\n      matrix:\n        version: [20, 22]\n    concurrency:\n      group: reusable-${{ github.ref }}\n      cancel-in-progress: true\n";
    let workflow = parse_workflow("t.yml", source).expect("valid reusable caller job parses");
    let call = &workflow.jobs[0];
    let unsupported = call
        .reusable_call
        .as_ref()
        .expect("reusable call is retained for planning rejection");
    assert_eq!(unsupported.name, "reusable workflow call (jobs.<id>.uses)");
    assert_eq!(unsupported.location.start.line, 4);
    assert_eq!(unsupported.location.start.column, 5);
}

#[test]
fn permission_scopes_and_levels_use_githubs_closed_schema() {
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

    for (scope, rejected, allowed, workflow_column, job_column) in [
        ("id-token", "read", "write|none", 13, 17),
        ("models", "write", "read|none", 11, 15),
        ("vulnerability-alerts", "write", "read|none", 25, 29),
        ("contents", "admin", "read|write|none", 13, 17),
    ] {
        let expected = format!("permission '{scope}' level '{rejected}' must be one of {allowed}");
        let workflow_source = format!(
            "{HEADER}permissions:\n  {scope}: {rejected}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
        );
        let job_source = format!(
            "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    permissions:\n      {scope}: {rejected}\n    steps:\n      - run: echo hi\n"
        );
        for (level, source, line, column) in [
            ("workflow", workflow_source, 3, workflow_column),
            ("job", job_source, 6, job_column),
        ] {
            match parse_workflow("t.yml", &source) {
                Err(ParseError::Schema { span, message }) => {
                    assert_eq!(span.start.line, line, "{level} {scope}");
                    assert_eq!(span.start.column, column, "{level} {scope}");
                    assert_eq!(message, expected, "{level} {scope}");
                }
                other => panic!("{level} {scope} must reject {rejected}, got {other:?}"),
            }
        }
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
