//! Documented built-in and status-function oracle rows.

use crate::error::ParseError;
use crate::{
    DEFAULT_MAX_MEMORY_BYTES, EvalError, EvaluationOptions, RunStatus, Value,
    WORKFLOW_TEMPLATE_MAX_MEMORY_BYTES, evaluate, evaluate_with_options, parse,
};

use super::{ctx, eval};

mod from_json;

#[test]
fn documented_builtin_function_examples_and_rules() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#functions
    let labels = Value::array(vec![
        Value::object(vec![("name".into(), Value::String("bug".into()))]),
        Value::object(vec![("name".into(), Value::String("help wanted".into()))]),
    ]);
    let context = ctx()
        .with_github(Value::object(vec![
            ("event_name".into(), Value::String("push".into())),
            (
                "event".into(),
                Value::object(vec![(
                    "issue".into(),
                    Value::object(vec![("labels".into(), labels)]),
                )]),
            ),
        ]))
        .with_env(Value::object(vec![
            ("continue".into(), Value::String("true".into())),
            ("time".into(), Value::String("3".into())),
        ]))
        .with_job(Value::object(vec![(
            "status".into(),
            Value::String("success".into()),
        )]));

    let matrix = Value::object(vec![(
        "include".into(),
        Value::array(vec![
            Value::object(vec![
                ("project".into(), Value::String("foo".into())),
                ("config".into(), Value::String("Debug".into())),
            ]),
            Value::object(vec![
                ("project".into(), Value::String("bar".into())),
                ("config".into(), Value::String("Release".into())),
            ]),
        ]),
    )]);
    let matrix_json =
        r#"{"include":[{"project":"foo","config":"Debug"},{"project":"bar","config":"Release"}]}"#;
    // The runner's DictionaryContextData uses ordinal-ignore-case keys and
    // replacing a differently-cased duplicate preserves the first spelling.
    // https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ContextData/DictionaryContextData.cs
    let mut rows = vec![
        (
            "contains('Hello world', 'llo')".to_string(),
            Value::Bool(true),
        ),
        ("contains('café', 'FÉ')".to_string(), Value::Bool(true)),
        (
            "contains(github.event.issue.labels.*.name, 'bug')".to_string(),
            Value::Bool(true),
        ),
        (
            r#"contains(fromJSON('["push", "pull_request"]'), github.event_name)"#.to_string(),
            Value::Bool(true),
        ),
        (
            r#"contains(fromJSON('["PUSH"]'), 'push')"#.to_string(),
            Value::Bool(true),
        ),
        (
            "startsWith('Hello world', 'he')".to_string(),
            Value::Bool(true),
        ),
        ("startsWith('Árvore', 'ár')".to_string(), Value::Bool(true)),
        (
            "endsWith('Hello world', 'LD')".to_string(),
            Value::Bool(true),
        ),
        ("endsWith('CAFÉ', 'fé')".to_string(), Value::Bool(true)),
        (
            "format('Hello {0} {1} {2}', 'Mona', 'the', 'Octocat')".to_string(),
            Value::String("Hello Mona the Octocat".into()),
        ),
        (
            "format('{{Hello {0} {1} {2}!}}', 'Mona', 'the', 'Octocat')".to_string(),
            Value::String("{Hello Mona the Octocat!}".into()),
        ),
        (
            "format('literal only')".to_string(),
            Value::String("literal only".into()),
        ),
        (
            "join(github.event.issue.labels.*.name, ', ')".to_string(),
            Value::String("bug, help wanted".into()),
        ),
        (
            "join('Hello world', ', ')".to_string(),
            Value::String("Hello world".into()),
        ),
        (
            "toJSON(job)".to_string(),
            Value::String("{\n  \"status\": \"success\"\n}".into()),
        ),
        (
            r#"toJSON(fromJSON('["foo","bar"]'))"#.to_string(),
            Value::String("[\n  \"foo\",\n  \"bar\"\n]".into()),
        ),
        ("fromJSON(env.continue)".to_string(), Value::Bool(true)),
        ("fromJSON(env.time)".to_string(), Value::Number(3.0)),
        ("fromJSON('null')".to_string(), Value::Null),
        (
            r#"fromJSON('{"A":1,"a":2}').A"#.to_string(),
            Value::Number(2.0),
        ),
        (
            r#"fromJSON('{"A":1,"a":2}').a"#.to_string(),
            Value::Number(2.0),
        ),
        (
            r#"toJSON(fromJSON('{"A":1,"a":2}'))"#.to_string(),
            Value::String("{\n  \"A\": 2\n}".into()),
        ),
        (format!("fromJSON('{matrix_json}')"), matrix),
    ];

    let replacement_values = (0..254)
        .map(|index| format!("'{index}'"))
        .collect::<Vec<_>>()
        .join(", ");
    rows.push((
        format!("format('{{253}}', {replacement_values})"),
        Value::String("253".into()),
    ));

    for (source, expected) in rows {
        assert_eq!(eval(&source, &context), expected, "function row {source}");
    }

    let too_many_replacements = (0..255)
        .map(|index| format!("'{index}'"))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(matches!(
        parse(&format!("format('{{254}}', {too_many_replacements})")),
        Err(ParseError::WrongArity { .. })
    ));
}

#[test]
fn evaluation_memory_limit_matches_runner_format_accounting_boundary() {
    // EvaluationContext defaults MaxMemory to exactly 1,048,576 bytes.
    // MemoryCounter estimates every appended .NET string as 26 bytes plus
    // two bytes per UTF-16 code unit, and rejects only when the total is
    // greater than the limit. FormatResultBuilder counts every placeholder
    // occurrence as a separate appended string.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/EvaluationContext.cs#L25-L36
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/MemoryCounter.cs#L47-L85
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Format.cs#L227-L270
    let argument = "x".repeat(1_011);
    let context = ctx().with_github(Value::object(vec![(
        "value".into(),
        Value::String(argument.clone()),
    )]));
    let at_limit = format!("format('{}', github.value)", "{0}".repeat(512));
    let over_limit = format!("format('{}', github.value)", "{0}".repeat(513));
    // Each replacement accounts for 26 + 2*1,011 = 2,048 bytes, so 512
    // occurrences are exactly 1 MiB and the 513th crosses the boundary.
    let accepted = parse(&at_limit)
        .unwrap_or_else(|error| panic!("at-limit format expression did not parse: {error}"));
    let accepted_value = evaluate(&accepted, &context)
        .unwrap_or_else(|error| panic!("exactly-at-limit format failed: {error}"));
    assert_eq!(accepted_value, Value::String(argument.repeat(512)));

    let rejected = parse(&over_limit)
        .unwrap_or_else(|error| panic!("over-limit format expression did not parse: {error}"));
    assert!(matches!(
        evaluate(&rejected, &context),
        Err(EvalError::MemoryLimitExceeded { max_bytes })
            if max_bytes == DEFAULT_MAX_MEMORY_BYTES
    ));

    // Workflow template evaluation deliberately supplies its surrounding
    // 10 MiB budget, proving the standalone 1 MiB default is configurable
    // rather than incorrectly hard-coded into all workflow call paths.
    assert!(
        evaluate_with_options(
            &rejected,
            &context,
            EvaluationOptions::new(WORKFLOW_TEMPLATE_MAX_MEMORY_BYTES),
        )
        .is_ok()
    );

    // Indexing a PipelineContextData primitive has two allocations in
    // ExpressionNode.Evaluate: a 24-byte raw wrapper plus its canonical
    // string. The target object (24 bytes) and literal `value` index (36
    // bytes) remain live because a raw conversion result does not trim child
    // depths. This payload therefore lands the whole expression exactly on
    // the 1 MiB boundary: 24 + 36 + 24 + 26 + 2*524,233.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/ExpressionNode.cs#L88-L115
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/ExpressionUtility.cs#L58-L63
    let exact_context_value = "y".repeat(524_233);
    let exact_context = ctx().with_github(Value::object(vec![(
        "value".into(),
        Value::String(exact_context_value.clone()),
    )]));
    let property = parse("github.value")
        .unwrap_or_else(|error| panic!("context-property expression did not parse: {error}"));
    assert_eq!(
        evaluate(&property, &exact_context)
            .unwrap_or_else(|error| panic!("exact context boundary failed: {error}")),
        Value::String(exact_context_value.clone())
    );
    let oversized_context = ctx().with_github(Value::object(vec![(
        "value".into(),
        Value::String(format!("{exact_context_value}y")),
    )]));
    assert!(matches!(
        evaluate(&property, &oversized_context),
        Err(EvalError::MemoryLimitExceeded { max_bytes })
            if max_bytes == DEFAULT_MAX_MEMORY_BYTES
    ));
}

#[test]
fn documented_case_examples() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#case
    let main = ctx().with_github(Value::object(vec![(
        "ref".into(),
        Value::String("refs/heads/main".into()),
    )]));
    let staging = ctx().with_github(Value::object(vec![(
        "ref".into(),
        Value::String("refs/heads/staging".into()),
    )]));
    let feature = ctx().with_github(Value::object(vec![(
        "ref".into(),
        Value::String("refs/heads/feature/topic".into()),
    )]));
    let other = ctx().with_github(Value::object(vec![(
        "ref".into(),
        Value::String("refs/heads/docs".into()),
    )]));
    let single = "case(github.ref == 'refs/heads/main', 'production', 'development')";
    let multiple = "case(github.ref == 'refs/heads/main', 'production', github.ref == 'refs/heads/staging', 'staging', startsWith(github.ref, 'refs/heads/feature/'), 'development', 'unknown')";
    let rows = [
        ("single predicate match", single, &main, "production"),
        ("single predicate default", single, &other, "development"),
        ("multiple predicates main", multiple, &main, "production"),
        ("multiple predicates staging", multiple, &staging, "staging"),
        (
            "multiple predicates feature",
            multiple,
            &feature,
            "development",
        ),
        ("multiple predicates default", multiple, &other, "unknown"),
    ];
    for (name, source, context, expected) in rows {
        assert_eq!(
            eval(source, context),
            Value::String(expected.into()),
            "case row {name}"
        );
    }
}

#[test]
fn documented_status_function_examples_and_truth_table() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#status-check-functions
    let succeeding = ctx().with_status(RunStatus::Success);
    let failing = ctx()
        .with_status(RunStatus::Failure)
        .with_steps(Value::object(vec![(
            "demo".into(),
            Value::object(vec![("conclusion".into(), Value::String("failure".into()))]),
        )]));
    let cancelling = ctx().with_status(RunStatus::Cancelled);
    let rows = [
        ("success is true", "success()", &succeeding, true),
        ("success is false", "success()", &failing, false),
        ("failure is true", "failure()", &failing, true),
        ("failure is false", "failure()", &succeeding, false),
        ("cancelled is true", "cancelled()", &cancelling, true),
        ("cancelled is false", "cancelled()", &succeeding, false),
        ("always after cancellation", "always()", &cancelling, true),
        (
            "failure with a step condition",
            "failure() && steps.demo.conclusion == 'failure'",
            &failing,
            true,
        ),
    ];
    for (name, source, context, expected) in rows {
        assert_eq!(
            eval(source, context),
            Value::Bool(expected),
            "status row {name}"
        );
    }
}
