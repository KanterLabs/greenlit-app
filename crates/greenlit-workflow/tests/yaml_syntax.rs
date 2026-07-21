//! Oracle table: GitHub workflow YAML document rules — duplicate mapping
//! keys, merge keys (`<<`), anchors/aliases, one document per file, and the
//! unknown-key policy.

use greenlit_workflow::{Location, ParseError, Span, parse_workflow};

const HEADER: &str = "on: push\n";

#[test]
fn root_trigger_job_and_step_nodes_preserve_exact_source_spans() {
    let source = concat!(
        "on: push\n",
        "jobs:\n",
        "  build:\n",
        "    runs-on: ubuntu-latest\n",
        "    steps:\n",
        "      - run: echo hi\n",
    );
    let workflow = parse_workflow("spans.yml", source).expect("parses");
    let span = |start_line, start_column, end_line, end_column| {
        Span::new(
            "spans.yml".into(),
            Location::new(start_line, start_column),
            Location::new(end_line, end_column),
        )
    };

    assert_eq!(workflow.span, span(1, 1, 7, 1));
    assert_eq!(workflow.on[0].span, span(1, 5, 1, 9));
    assert_eq!(workflow.jobs[0].span, span(4, 5, 7, 1));
    assert_eq!(workflow.jobs[0].steps[0].span, span(6, 9, 7, 1));
}

#[test]
fn duplicate_mapping_keys_are_rejected() {
    let source = format!(
        "{HEADER}env:\n  FOO: one\n  FOO: two\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(
        matches!(err, ParseError::DuplicateKey { .. }),
        "got {err:?}"
    );

    // `actions/runner` deduplicates the decoded `StringToken.Value`, not
    // the YAML scalar style, so quoted/plain spellings of the same value
    // collide as well:
    // https://github.com/actions/runner/blob/main/src/Sdk/DTObjectTemplating/ObjectTemplating/TemplateReader.cs
    let source = format!(
        "{HEADER}env:\n  'FOO': one\n  FOO: two\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("quote style must not distinguish keys");
    assert!(
        matches!(err, ParseError::DuplicateKey { ref key, .. } if key == "FOO"),
        "got {err:?}"
    );
}

#[test]
fn merge_keys_are_not_supported_and_surface_as_unknown_key() {
    // GitHub treats `<<` as an ordinary (and therefore unrecognized) key
    // rather than performing a YAML-1.1 merge; its template reader consumes
    // decoded mapping keys without applying merge semantics:
    // https://github.com/actions/runner/blob/main/src/Sdk/DTObjectTemplating/ObjectTemplating/TemplateReader.cs
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    <<: {{}}\n    steps:\n      - run: echo hi\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(
        matches!(err, ParseError::UnknownKey { ref key, .. } if key == "<<"),
        "got {err:?}"
    );
}

#[test]
fn anchors_and_aliases_are_resolved() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    env: &common\n      FOO: bar\n    steps:\n      - run: echo hi\n        env: *common\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("anchors/aliases must parse");
    let step_env = &workflow.jobs[0].steps[0].env;
    assert_eq!(step_env[0].0.value, "FOO");
}

#[test]
fn exactly_one_document_is_required() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n---\non: push\njobs:\n  x:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(
        matches!(err, ParseError::MultipleDocuments { count: 2, .. }),
        "got {err:?}"
    );
}

#[test]
fn an_empty_file_is_rejected() {
    let err = parse_workflow("t.yml", "").expect_err("must fail");
    assert!(
        matches!(err, ParseError::EmptyDocument { .. }),
        "got {err:?}"
    );
}

#[test]
fn unknown_top_level_key_is_rejected() {
    let source = format!(
        "{HEADER}run-name: build\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(
        matches!(err, ParseError::UnknownKey { ref key, .. } if key == "run-name"),
        "got {err:?}"
    );
}

#[test]
fn unknown_job_level_key_is_rejected() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    permissions:\n      contents: read\n    steps:\n      - run: echo hi\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(
        matches!(err, ParseError::UnknownKey { ref key, .. } if key == "permissions"),
        "got {err:?}"
    );
}

#[test]
fn malformed_yaml_is_a_yaml_syntax_error() {
    let source = "on: [push\njobs: {}\n";
    let err = parse_workflow("t.yml", source).expect_err("must fail");
    assert!(matches!(err, ParseError::Yaml { .. }), "got {err:?}");
}
