//! End-to-end, table-driven oracle rows transcribed from GitHub's public
//! [Expressions reference](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions)
//! and [Contexts reference](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts).
//! Every row enters through the public `parse` then `evaluate` path; the
//! filesystem fake replaces only the true filesystem boundary used by
//! `hashFiles()`.

use std::sync::Arc;

use crate::functions::hash_files::test_support::NoFiles;
use crate::{Context, ParseError, Value, evaluate, parse};

mod functions;
mod hash_files;

fn ctx() -> Context {
    Context::new(Arc::new(NoFiles::new("/workspace")))
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
fn expression_limits_reject_deep_input_without_exhausting_the_process_stack() {
    // The runner uses an iterative parser followed by a MaxDepth=50 AST
    // check. These inputs pin the same semantic depth boundary while also
    // proving that every long-chain shape accepted by the 21,000 UTF-16-unit
    // lexer limit is handled without recursive stack exhaustion.
    // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionParser.cs
    let forty_nine_nots = format!("{}true", "!".repeat(49));
    assert!(parse(&forty_nine_nots).is_ok());

    let twenty_thousand_nots = format!("{}true", "!".repeat(20_000));
    assert!(matches!(
        parse(&twenty_thousand_nots),
        Err(ParseError::TooDeep)
    ));

    let long_logical_chain = format!("{}true", "false || ".repeat(2_000));
    assert_eq!(eval(&long_logical_chain, &ctx()), Value::Bool(true));

    let long_postfix_chain = format!("github{}", ".missing".repeat(100));
    assert!(matches!(
        parse(&long_postfix_chain),
        Err(ParseError::TooDeep)
    ));

    let long_equality_chain = format!("{}true", "true == ".repeat(100));
    assert!(matches!(
        parse(&long_equality_chain),
        Err(ParseError::TooDeep)
    ));

    // Parentheses are transparent to GitHub's AST depth and remain accepted
    // all the way to the source-length boundary.
    let excessive_grouping = format!("{}true{}", "(".repeat(129), ")".repeat(129));
    assert_eq!(eval(&excessive_grouping, &ctx()), Value::Bool(true));

    let maximum_length_grouping = format!("{}true{}", "(".repeat(10_498), ")".repeat(10_498));
    assert_eq!(
        maximum_length_grouping.encode_utf16().count(),
        crate::error::MAX_EXPRESSION_LENGTH
    );
    assert_eq!(eval(&maximum_length_grouping, &ctx()), Value::Bool(true));
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
    // The runner evaluates an index target first and does not evaluate the
    // key when that target is null or primitive:
    // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Operators/Index.cs
    let context = ctx().with_github(Value::object(vec![(
        "ref".into(),
        Value::String("refs/heads/main".into()),
    )]));
    let rows = [
        ("(true)", Value::Bool(true)),
        ("fromJSON('[10,20]')[1]", Value::Number(20.0)),
        ("github.ref", Value::String("refs/heads/main".into())),
        ("fromJSON('\"value\"')[fromJSON('bad')]", Value::Null),
        ("github.missing[fromJSON('bad')]", Value::Null),
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
    // `Index.cs` returns null for a missing object property. The documented
    // empty text is its later string conversion; retaining Null here matters
    // observably to `toJSON`, which writes it as the JSON token `null`.
    // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Operators/Index.cs
    // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/ToJson.cs
    // The Actions runner uses CaseSensitiveDictionaryContextData for `env`
    // on Linux, matching GitHub's guidance to treat environment-variable
    // names as case-sensitive:
    // https://github.com/actions/runner/blob/main/src/Runner.Worker/ExecutionContext.cs
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands#setting-an-environment-variable
    let context = Context::new(Arc::new(NoFiles::new("/workspace")))
        .with_github(Value::object(vec![
            ("G".into(), Value::String("g".into())),
            ("ÁCCENT".into(), Value::String("unicode-key".into())),
        ]))
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
        ("github['áccent']", Value::String("unicode-key".into())),
        ("env.E", Value::String("e".into())),
        ("env.e", Value::Null),
        ("vars.V", Value::String("v".into())),
        ("secrets.S", Value::String("s".into())),
        ("needs.N", Value::String("n".into())),
        ("matrix.M", Value::String("m".into())),
        ("strategy.Y", Value::String("y".into())),
        ("steps.T", Value::String("t".into())),
        ("runner.R", Value::String("r".into())),
        ("job.J", Value::String("j".into())),
        ("inputs.I", Value::String("i".into())),
        ("github.missing", Value::Null),
        ("toJSON(github.missing)", Value::String("null".into())),
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
        ("'á' == 'Á'", true),
        ("'á' == 'à'", false),
        ("'ı' == 'I'", false),
        ("'ß' == 'SS'", false),
        ("'à' < 'Á'", true),
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
