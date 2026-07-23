//! Oracle table: GitHub workflow YAML document rules — duplicate mapping
//! keys, merge keys (`<<`), anchors/aliases, one document per file, and the
//! unknown-key policy.

use greenlit_workflow::{
    Location, MAX_WORKFLOW_SOURCE_CHARACTERS, ParseError, Span,
    model::{
        job::{MatrixSource, RunsOn},
        trigger::Trigger,
        value::{ScalarOrExpr, YamlScalar, YamlValue},
    },
    parse_workflow,
};

const HEADER: &str = "on: push\n";

#[test]
fn root_mapped_triggers_inputs_job_and_step_nodes_preserve_exact_source_spans() {
    let source = concat!(
        "on:\n",
        "  push:\n",
        "    branches: [main]\n",
        "  workflow_dispatch:\n",
        "    inputs:\n",
        "      target:\n",
        "        description: Deploy target\n",
        "        type: string\n",
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

    assert_eq!(workflow.span, span(1, 1, 14, 1));
    assert_eq!(workflow.jobs[0].span, span(11, 5, 14, 1));
    assert_eq!(workflow.jobs[0].steps[0].span, span(13, 9, 14, 1));
    assert_eq!(workflow.jobs[0].id.span, span(10, 3, 10, 8));

    let Trigger::Webhook { filter, .. } = &workflow.on[0].value else {
        panic!("expected mapped push trigger");
    };
    assert_eq!(workflow.on[0].span, filter.span);
    assert!(filter.span.start <= filter.branches[0].span.start);
    assert!(filter.span.end >= filter.branches[0].span.end);

    let Trigger::WorkflowDispatch(dispatch) = &workflow.on[1].value else {
        panic!("expected workflow_dispatch trigger");
    };
    let input = &dispatch.inputs[0];
    assert!(workflow.on[1].span.start <= input.span.start);
    assert!(workflow.on[1].span.end >= input.span.end);
    assert_ne!(input.span, input.name.span);
    assert!(input.span.start <= input.description.as_ref().expect("description").span.start);
    assert!(input.span.end >= input.input_type.as_ref().expect("type").span.end);
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

    // TemplateReader uses StringComparer.OrdinalIgnoreCase for both its
    // fixed-schema and loose-property maps. Case variants therefore collide
    // for repository-defined names too (job ids are the representative loose
    // mapping here), rather than becoming two local jobs.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/ObjectTemplating/TemplateReader.cs#L302-L342
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo one\n  BUILD:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo two\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("key case must not distinguish jobs");
    assert!(
        matches!(err, ParseError::DuplicateKey { ref key, .. } if key == "BUILD"),
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
        "{HEADER}jobs:\n  build:\n    runs-on: &labels [self-hosted, linux]\n    env: &common\n      FOO: bar\n    steps:\n      - run: echo hi\n        env: *common\n  test:\n    runs-on: *labels\n    steps:\n      - run: echo test\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("anchors/aliases must parse");
    let step_env = &workflow.jobs[0].steps[0].env;
    assert_eq!(step_env[0].0.value, "FOO");
    // An alias gets the use-site's outer span, while children retain the
    // anchor definition's spans, matching `saphyr` and the runner-facing
    // workflow model contract.
    assert_eq!(step_env[0].0.span.start.line, 6);
    let runs_on = workflow.jobs[1].runs_on.as_ref().expect("runs-on alias");
    assert_eq!(runs_on.span.start.line, 11);
    let RunsOn::Labels(labels) = &runs_on.value else {
        panic!("expected aliased label sequence");
    };
    assert_eq!(labels[0].span.start.line, 4);
}

#[test]
fn collection_tags_are_ignored_by_the_runner_reader() {
    // `AllowSequenceStart` and `AllowMappingStart` dispatch only on event
    // type and never inspect the collection event's tag. This applies even
    // to custom and shape-mismatched tags.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/Conversion/YamlObjectReader.cs#L114-L149
    for (name, job_fields) in [
        (
            "custom sequence tag",
            "    runs-on: !custom [ubuntu-latest]\n",
        ),
        (
            "mapping tag on sequence",
            "    runs-on: !!map [ubuntu-latest]\n",
        ),
        (
            "custom mapping tag",
            "    runs-on: ubuntu-latest\n    env: !custom {KEY: value}\n",
        ),
        (
            "sequence tag on mapping",
            "    runs-on: ubuntu-latest\n    env: !!seq {KEY: value}\n",
        ),
    ] {
        let source =
            format!("{HEADER}jobs:\n  build:\n{job_fields}    steps:\n      - run: echo hi\n");
        parse_workflow("t.yml", &source)
            .unwrap_or_else(|error| panic!("{name} must be ignored, got {error}"));
    }
}

#[test]
fn yaml_resource_limits_match_the_pinned_github_runner() {
    // The runner counts each event replayed through an alias and rejects
    // after 50,000, before materializing an unbounded alias-expanded tree.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/Conversion/YamlObjectReader.cs
    let mut alias_bomb = String::from("on: push\n");
    alias_bomb.push_str("value0: &value0 [x, x, x, x, x, x, x, x, x, x]\n");
    for level in 1..=5 {
        alias_bomb.push_str(&format!(
            "value{level}: &value{level} [*value{}, *value{}, *value{}, *value{}, *value{}, *value{}, *value{}, *value{}, *value{}, *value{}]\n",
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
        ));
    }
    alias_bomb.push_str("jobs: {}\n");
    let error = parse_workflow("aliases.yml", &alias_bomb).expect_err("alias bomb must fail");
    match error {
        ParseError::YamlLimit { span, message } => {
            assert_eq!(span.file.as_ref(), "aliases.yml");
            assert!(message.contains("50000"), "{message}");
            assert!(message.contains("aliases"), "{message}");
        }
        other => panic!("expected alias traversal limit, got {other:?}"),
    }

    // ParseOptions.MaxDepth is 50 collection elements. This check happens
    // while processing start events, so later recursive model conversion
    // never receives an over-deep raw tree.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/ParseOptions.cs
    let nested = format!("on: {}x{}\njobs: {{}}\n", "[".repeat(51), "]".repeat(51));
    let error = parse_workflow("depth.yml", &nested).expect_err("depth must fail");
    match error {
        ParseError::YamlLimit { span, message } => {
            assert_eq!(span.file.as_ref(), "depth.yml");
            assert!(span.start.line >= 1);
            assert!(message.contains("depth of 50"), "{message}");
        }
        other => panic!("expected YAML depth limit, got {other:?}"),
    }

    // MaxFileSize is measured in .NET characters (UTF-16 code units), not
    // UTF-8 bytes. One supplementary scalar consumes two units.
    let oversized = "\u{1f600}".repeat(MAX_WORKFLOW_SOURCE_CHARACTERS / 2 + 1);
    let error = parse_workflow("large.yml", &oversized).expect_err("size must fail");
    assert!(
        matches!(error, ParseError::SourceLimit { max_characters, .. } if max_characters == MAX_WORKFLOW_SOURCE_CHARACTERS),
        "got {error:?}"
    );
}

fn nested_anchor_matrix(alias_uses: usize) -> String {
    const ANCHORED_MAPPING_LEVELS: usize = 43;
    const LARGE_SCALAR_CHARACTERS: usize = 1_000_000;

    let mut source = String::from(
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        include:\n          - payload: &level0\n",
    );
    for level in 1..ANCHORED_MAPPING_LEVELS {
        source.push_str(&" ".repeat(14 + (level - 1) * 2));
        source.push_str(&format!("child: &level{level}\n"));
    }
    let leaf_indent = " ".repeat(14 + (ANCHORED_MAPPING_LEVELS - 1) * 2);
    source.push_str(&leaf_indent);
    source.push_str("leaf0: &large ");
    source.push_str(&"x".repeat(LARGE_SCALAR_CHARACTERS));
    source.push('\n');
    for alias in 1..=alias_uses {
        source.push_str(&leaf_indent);
        source.push_str(&format!("leaf{alias}: *large\n"));
    }
    source.push_str("    steps:\n      - run: echo hi\n");
    source
}

#[test]
fn nested_anchors_at_the_depth_and_result_boundary_are_resolved() {
    // Seven schema collections precede `payload`; 43 anchored mappings put
    // the innermost mapping at the runner's exact depth limit of 50. The
    // one-million-character scalar plus four aliases accounts for just over
    // 10,000,000 of the runner's 10 MiB logical result budget. Nested anchor
    // retention must stay shallow even though each replay is still charged
    // its full logical size.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/Conversion/YamlObjectReader.cs
    let source = nested_anchor_matrix(4);
    let workflow = parse_workflow("nested-anchors.yml", &source).expect("must fit runner limits");
    let strategy = workflow.jobs[0].strategy.as_ref().expect("strategy");
    let matrix = match &strategy.value.matrix.as_ref().expect("matrix").value {
        MatrixSource::Inline(matrix) => matrix,
        MatrixSource::Expression(_) => panic!("expected inline matrix"),
    };
    let payload = &matrix.include[0].value[0].1.value;
    let mut current = payload;
    for level in 0..43 {
        let YamlValue::Mapping(entries) = current else {
            panic!("level {level} must remain a mapping");
        };
        if level < 42 {
            assert_eq!(entries[0].0.value, "child");
            current = &entries[0].1.value;
        } else {
            assert_eq!(entries.len(), 5);
            for (_, value) in entries {
                assert!(
                    matches!(
                        &value.value,
                        YamlValue::Scalar(ScalarOrExpr::Literal(YamlScalar::String(text)))
                            if text.len() == 1_000_000
                    ),
                    "alias leaf was not materialized faithfully"
                );
            }
        }
    }

    // One additional replay crosses 10 MiB. The public workflow parser must
    // reject at that alias instead of first cloning/materializing its large
    // subtree.
    let oversized = nested_anchor_matrix(5);
    let error =
        parse_workflow("nested-anchors.yml", &oversized).expect_err("result cap must apply");
    match error {
        ParseError::YamlLimit { span, message } => {
            assert_eq!(span.file.as_ref(), "nested-anchors.yml");
            assert!(message.contains("10485760"), "{message}");
            assert!(message.contains("result size"), "{message}");
        }
        other => panic!("expected YAML result-size limit, got {other:?}"),
    }
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
fn run_name_is_modeled_and_unknown_top_level_keys_are_rejected() {
    let source = format!(
        "{HEADER}run-name: Build ${{{{ github.ref }}}}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("run-name must parse");
    let run_name = workflow.run_name.expect("typed run-name");
    assert_eq!(run_name.value, "Build ${{ github.ref }}");
    assert_eq!(run_name.span.start, Location::new(2, 11));

    let source = format!(
        "{HEADER}mystery: build\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(
        matches!(err, ParseError::UnknownKey { ref key, .. } if key == "mystery"),
        "got {err:?}"
    );
}

#[test]
fn job_permissions_are_modeled_and_unknown_job_level_keys_are_rejected() {
    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    permissions:\n      contents: read\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("job permissions must parse");
    assert!(workflow.jobs[0].permissions.is_some());

    let source = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    mystery: value\n    steps:\n      - run: echo hi\n"
    );
    let err = parse_workflow("t.yml", &source).expect_err("must fail");
    assert!(
        matches!(err, ParseError::UnknownKey { ref key, .. } if key == "mystery"),
        "got {err:?}"
    );
}

#[test]
fn malformed_yaml_is_a_yaml_syntax_error() {
    let source = "on: [push\njobs: {}\n";
    let err = parse_workflow("t.yml", source).expect_err("must fail");
    assert!(matches!(err, ParseError::Yaml { .. }), "got {err:?}");
}
