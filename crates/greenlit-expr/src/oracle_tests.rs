//! End-to-end, table-driven oracle rows transcribed from GitHub's public
//! [Expressions reference](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions)
//! and [Contexts reference](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts).
//! Every row enters through the public `parse` then `evaluate` path; the
//! filesystem fake replaces only the true filesystem boundary used by
//! `hashFiles()`.

use std::rc::Rc;

use crate::error::ParseError;
use crate::functions::hash_files::test_support::NoFiles;
use crate::{Context, RunStatus, Value, evaluate, parse};

mod hash_files;

fn ctx() -> Context {
    Context::new(Rc::new(NoFiles::new("/workspace")))
}

fn eval(source: &str, context: &Context) -> Value {
    let expression = parse(source).unwrap_or_else(|error| {
        panic!("parse({source:?}) failed: {error}");
    });
    evaluate(&expression, context).unwrap_or_else(|error| {
        panic!("evaluate({source:?}) failed: {error}");
    })
}

fn eval_string(source: &str, context: &Context) -> String {
    match eval(source, context) {
        Value::String(value) => value,
        other => panic!("{source:?} produced {other:?}, expected String"),
    }
}

#[test]
fn documented_literals_and_conditional_truthiness() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#literals
    let context = ctx();
    let literal_rows = [
        ("true", Value::Bool(true)),
        ("false", Value::Bool(false)),
        ("null", Value::Null),
        ("711", Value::Number(711.0)),
        ("-9.2", Value::Number(-9.2)),
        ("0xff", Value::Number(255.0)),
        ("-2.99e-2", Value::Number(-2.99e-2)),
        (
            "'It''s open source!'",
            Value::String("It's open source!".into()),
        ),
    ];
    for (source, expected) in literal_rows {
        assert_eq!(eval(source, &context), expected, "literal row {source}");
    }

    let truthiness_rows = [
        ("!false", true),
        ("!0", true),
        ("!-0", true),
        ("!''", true),
        ("!fromJSON('\"\"')", true),
        ("!null", true),
        ("!true", false),
        ("!'false'", false),
        ("!fromJSON('[]')", false),
        ("!fromJSON('{}')", false),
    ];
    for (source, expected) in truthiness_rows {
        assert_eq!(
            eval(source, &context),
            Value::Bool(expected),
            "truthiness row {source}"
        );
    }

    assert!(
        parse("\"double quoted\"").is_err(),
        "double-quoted strings inside an expression are documented as invalid"
    );
}

#[test]
fn documented_operators_property_and_index_access() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#operators
    let context = ctx().with_github(Value::object(vec![(
        "ref".into(),
        Value::String("refs/heads/main".into()),
    )]));
    let rows = [
        ("(true)", Value::Bool(true)),
        ("fromJSON('[10,20]')[1]", Value::Number(20.0)),
        ("github.ref", Value::String("refs/heads/main".into())),
        ("!false", Value::Bool(true)),
        ("1 < 2", Value::Bool(true)),
        ("1 <= 1", Value::Bool(true)),
        ("2 > 1", Value::Bool(true)),
        ("2 >= 2", Value::Bool(true)),
        ("1 == 1", Value::Bool(true)),
        ("1 != 2", Value::Bool(true)),
        ("true && true", Value::Bool(true)),
        ("false || true", Value::Bool(true)),
    ];
    for (source, expected) in rows {
        assert_eq!(eval(source, &context), expected, "operator row {source}");
    }
}

#[test]
fn documented_context_roots_and_missing_property_value() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#about-contexts
    let context = Context::new(Rc::new(NoFiles::new("/workspace")))
        .with_github(Value::object(vec![("G".into(), Value::String("g".into()))]))
        .with_env(Value::object(vec![("E".into(), Value::String("e".into()))]))
        .with_vars(Value::object(vec![("V".into(), Value::String("v".into()))]))
        .with_secrets(Value::object(vec![("S".into(), Value::String("s".into()))]))
        .with_needs(Value::object(vec![("N".into(), Value::String("n".into()))]))
        .with_matrix(Value::object(vec![("M".into(), Value::String("m".into()))]))
        .with_strategy(Value::object(vec![("Y".into(), Value::String("y".into()))]))
        .with_steps(Value::object(vec![("T".into(), Value::String("t".into()))]))
        .with_runner(Value::object(vec![("R".into(), Value::String("r".into()))]))
        .with_job(Value::object(vec![("J".into(), Value::String("j".into()))]))
        .with_inputs(Value::object(vec![("I".into(), Value::String("i".into()))]));
    let rows = [
        ("github.g", Value::String("g".into())),
        ("env.E", Value::String("e".into())),
        ("vars.V", Value::String("v".into())),
        ("secrets.S", Value::String("s".into())),
        ("needs.N", Value::String("n".into())),
        ("matrix.M", Value::String("m".into())),
        ("strategy.Y", Value::String("y".into())),
        ("steps.T", Value::String("t".into())),
        ("runner.R", Value::String("r".into())),
        ("job.J", Value::String("j".into())),
        ("inputs.I", Value::String("i".into())),
        ("github.missing", Value::String(String::new())),
    ];
    for (source, expected) in rows {
        assert_eq!(eval(source, &context), expected, "context row {source}");
    }
}

#[test]
fn documented_object_filter_examples() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#object-filters
    let context = ctx();
    let rows = [
        (
            "array filter",
            r#"fromJSON('[{"name":"apple","quantity":1},{"name":"orange","quantity":2},{"name":"pear","quantity":1}]').*.name"#,
            vec![vec!["apple"], vec!["orange"], vec!["pear"]],
            true,
        ),
        (
            "object filter",
            r#"fromJSON('{"scallions":{"colors":["green","white","red"],"ediblePortions":["roots","stalks"]},"beets":{"colors":["purple","red","gold","white","pink"],"ediblePortions":["roots","stems","leaves"]},"artichokes":{"colors":["green","purple","red","black"],"ediblePortions":["hearts","stems","leaves"]}}').*.ediblePortions"#,
            vec![
                vec!["roots", "stalks"],
                vec!["roots", "stems", "leaves"],
                vec!["hearts", "stems", "leaves"],
            ],
            false,
        ),
    ];

    for (name, source, expected, order_is_defined) in rows {
        let Value::Array(values) = eval(source, &context) else {
            panic!("{name} did not produce an array");
        };
        let mut actual = values
            .items()
            .iter()
            .map(|value| match value {
                Value::String(value) => vec![value.clone()],
                Value::Array(nested) => nested
                    .items()
                    .iter()
                    .map(|item| match item {
                        Value::String(value) => value.clone(),
                        other => panic!("{name} produced non-string nested item {other:?}"),
                    })
                    .collect(),
                other => panic!("{name} produced unexpected item {other:?}"),
            })
            .collect::<Vec<Vec<String>>>();
        let mut expected = expected
            .into_iter()
            .map(|group| group.into_iter().map(str::to_string).collect())
            .collect::<Vec<Vec<String>>>();
        if !order_is_defined {
            actual.sort();
            expected.sort();
        }
        assert_eq!(actual, expected, "object-filter row {name}");
    }
}

#[test]
fn documented_loose_equality_and_relational_coercion() {
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#operators
    let shared_array = Value::array(vec![Value::Number(1.0)]);
    let shared_object = Value::object(vec![("value".into(), Value::Number(1.0))]);
    let context = ctx().with_github(Value::object(vec![
        ("array".into(), shared_array.clone()),
        ("array_alias".into(), shared_array),
        ("object".into(), shared_object.clone()),
        ("object_alias".into(), shared_object),
    ]));
    let rows = [
        ("null == 0", true),
        ("false == 0", true),
        ("true == 1", true),
        ("'' == 0", true),
        ("'-2.99e-2' == -2.99e-2", true),
        ("'not a number' == 0", false),
        ("fromJSON('[]') < 1", false),
        ("fromJSON('{}') < 1", false),
        ("'not a number' < 1", false),
        ("'not a number' <= 1", false),
        ("'not a number' > 1", false),
        ("'not a number' >= 1", false),
        ("'Alpha' == 'alpha'", true),
        ("'ALPHA' < 'beta'", true),
        ("'BETA' > 'alpha'", true),
        ("'ALPHA' <= 'alpha'", true),
        ("'ALPHA' >= 'alpha'", true),
        ("github.array == github.array_alias", true),
        ("github.array == fromJSON('[1]')", false),
        ("github.object == github.object_alias", true),
        ("github.object == fromJSON('{\"value\":1}')", false),
    ];
    for (source, expected) in rows {
        assert_eq!(
            eval(source, &context),
            Value::Bool(expected),
            "coercion row {source}"
        );
    }
}

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
    let mut rows = vec![
        (
            "contains('Hello world', 'llo')".to_string(),
            Value::Bool(true),
        ),
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
        (
            "endsWith('Hello world', 'LD')".to_string(),
            Value::Bool(true),
        ),
        (
            "format('Hello {0} {1} {2}', 'Mona', 'the', 'Octocat')".to_string(),
            Value::String("Hello Mona the Octocat".into()),
        ),
        (
            "format('{{Hello {0} {1} {2}!}}', 'Mona', 'the', 'Octocat')".to_string(),
            Value::String("{Hello Mona the Octocat!}".into()),
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
        (format!("fromJSON('{matrix_json}')"), matrix),
    ];

    let replacement_values = (0..=256)
        .map(|index| format!("'{index}'"))
        .collect::<Vec<_>>()
        .join(", ");
    rows.push((
        format!("format('{{256}}', {replacement_values})"),
        Value::String("256".into()),
    ));

    for (source, expected) in rows {
        assert_eq!(eval(&source, &context), expected, "function row {source}");
    }

    assert!(matches!(
        parse("format('missing replacement value')"),
        Err(ParseError::WrongArity { .. })
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
