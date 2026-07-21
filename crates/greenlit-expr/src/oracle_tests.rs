//! End-to-end oracle rows transcribed from GitHub's Expressions reference
//! and runner/toolkit source. Every language behavior enters through the
//! public `parse` then `evaluate` path; the filesystem fake replaces only
//! the true filesystem boundary used by `hashFiles()`.

use std::rc::Rc;

use crate::error::{EvalError, ParseError};
use crate::functions::hash_files::test_support::NoFiles;
use crate::{Context, RunStatus, Value, evaluate, expr_calls_status_function, parse};

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
fn literals_lexing_and_parser_limits() {
    let context = ctx();
    let rows = [
        ("true", Value::Bool(true)),
        ("false", Value::Bool(false)),
        ("null", Value::Null),
        ("42", Value::Number(42.0)),
        ("-9.2", Value::Number(-9.2)),
        ("0xff", Value::Number(255.0)),
        ("0o17", Value::Number(15.0)),
        ("'it''s'", Value::String("it's".into())),
    ];
    for (source, expected) in rows {
        assert_eq!(eval(source, &context), expected, "row {source}");
    }
    assert!(parse("\"double quoted\"").is_err());
    assert!(
        parse("0Xff").is_err(),
        "the runner recognizes lowercase 0x only"
    );

    let too_long = format!("'{}'", "😀".repeat(10_500));
    assert!(matches!(
        parse(&too_long),
        Err(ParseError::TooLong { actual: 21_002 })
    ));
    let at_limit = format!("'{}'", "😀".repeat(10_499));
    assert!(parse(&at_limit).is_ok(), "21,000 UTF-16 units are accepted");
}

#[test]
fn operators_precedence_postfix_and_completed_ast_depth() {
    let context = ctx().with_github(Value::object(vec![(
        "event".into(),
        Value::object(vec![(
            "pull_request".into(),
            Value::object(vec![("number".into(), Value::Number(5.0))]),
        )]),
    )]));
    let rows = [
        ("!false", Value::Bool(true)),
        ("1 != 2", Value::Bool(true)),
        ("1 <= 1", Value::Bool(true)),
        ("2 > 1", Value::Bool(true)),
        ("2 >= 2", Value::Bool(true)),
        ("true && 'right'", Value::String("right".into())),
        ("false && 'right'", Value::Bool(false)),
        ("'left' || 'right'", Value::String("left".into())),
        ("'' || 'right'", Value::String("right".into())),
        ("1 == 2 < 3", Value::Bool(true)),
        ("(1 == 1)", Value::Bool(true)),
        ("github.event.pull_request.number", Value::Number(5.0)),
        (
            "github['event']['pull_request']['number']",
            Value::Number(5.0),
        ),
        ("('abc')[0]", Value::Null),
    ];
    for (source, expected) in rows {
        assert_eq!(eval(source, &context), expected, "row {source}");
    }
    assert!(
        parse("'abc'[0]").is_err(),
        "a bare literal is not postfixable"
    );

    let grouped = format!("{}true{}", "(".repeat(51), ")".repeat(51));
    assert!(parse(&grouped).is_ok(), "grouping is absent from the AST");
    let flat_and = std::iter::repeat_n("true", 60)
        .collect::<Vec<_>>()
        .join(" && ");
    assert!(
        parse(&flat_and).is_ok(),
        "the runner flattens one logical And container"
    );
    let depth_50 = format!("github{}", ".a".repeat(49));
    assert!(parse(&depth_50).is_ok());
    let depth_51 = format!("github{}", ".a".repeat(50));
    assert!(matches!(parse(&depth_51), Err(ParseError::TooDeep)));
}

#[test]
fn contexts_missing_keys_and_ordinal_ignore_case() {
    let context = Context::new(Rc::new(NoFiles::new("/workspace")))
        .with_github(Value::object(vec![(
            "I".into(),
            Value::String("ascii".into()),
        )]))
        .with_env(Value::object(vec![(
            "FOO".into(),
            Value::String("bar".into()),
        )]))
        .with_vars(Value::object(vec![("V".into(), Value::String("v".into()))]))
        .with_secrets(Value::object(vec![("S".into(), Value::String("s".into()))]))
        .with_needs(Value::object(vec![("N".into(), Value::String("n".into()))]))
        .with_matrix(Value::object(vec![("M".into(), Value::String("m".into()))]))
        .with_steps(Value::object(vec![("T".into(), Value::String("t".into()))]))
        .with_runner(Value::object(vec![("R".into(), Value::String("r".into()))]))
        .with_job(Value::object(vec![("J".into(), Value::String("j".into()))]))
        .with_inputs(Value::object(vec![("P".into(), Value::String("p".into()))]));
    let rows = [
        ("github.i", Value::String("ascii".into())),
        ("env.FOO", Value::String("bar".into())),
        ("vars.V", Value::String("v".into())),
        ("secrets.S", Value::String("s".into())),
        ("needs.N", Value::String("n".into())),
        ("matrix.M", Value::String("m".into())),
        ("steps.T", Value::String("t".into())),
        ("runner.R", Value::String("r".into())),
        ("job.J", Value::String("j".into())),
        ("inputs.P", Value::String("p".into())),
        ("github.missing", Value::Null),
        ("github['ı']", Value::Null),
        ("'I' == 'ı'", Value::Bool(false)),
        ("contains('I', 'ı')", Value::Bool(false)),
        ("startsWith('I', 'ı')", Value::Bool(false)),
        ("endsWith('I', 'ı')", Value::Bool(false)),
        ("'I' < 'ı'", Value::Bool(true)),
    ];
    for (source, expected) in rows {
        assert_eq!(eval(source, &context), expected, "row {source}");
    }
}

#[test]
fn object_filter_rows_include_the_documented_object_shape() {
    let context = ctx();
    let names = eval(
        r#"fromJSON('{"fruits":[{"name":"apple"},{"name":"orange"},{"name":"pear"}]}').fruits.*.name"#,
        &context,
    );
    assert_eq!(
        names,
        Value::filtered_array(vec![
            Value::String("apple".into()),
            Value::String("orange".into()),
            Value::String("pear".into()),
        ])
    );

    let portions = eval(
        r#"fromJSON('{"vegetables":{"carrot":{"ediblePortions":["roots"]},"celery":{"ediblePortions":["stalks","leaves"]}}}').vegetables.*.ediblePortions"#,
        &context,
    );
    assert_eq!(
        portions,
        Value::filtered_array(vec![
            Value::array(vec![Value::String("roots".into())]),
            Value::array(vec![
                Value::String("stalks".into()),
                Value::String("leaves".into()),
            ]),
        ])
    );
}

#[test]
fn coercion_number_formatting_and_string_consumers() {
    let context = ctx();
    let rows = [
        ("null == ''", Value::Bool(true)),
        ("null == 0", Value::Bool(true)),
        ("null == false", Value::Bool(true)),
        ("true == 1", Value::Bool(true)),
        ("true == '1'", Value::Bool(true)),
        ("true == 'true'", Value::Bool(false)),
        ("'' == 0", Value::Bool(true)),
        ("'abc' == 0", Value::Bool(false)),
        ("'ABC' == 'abc'", Value::Bool(true)),
        ("'infinity' == Infinity", Value::Bool(true)),
        ("'INFINITY' == Infinity", Value::Bool(true)),
        ("'+InFiNiTy' == Infinity", Value::Bool(true)),
        ("'-iNfInItY' == -Infinity", Value::Bool(true)),
        ("format('{0}', 0.00001)", Value::String("1E-05".into())),
        ("join(fromJSON('[0.00001]'))", Value::String("1E-05".into())),
        ("contains(0.00001, 'E-05')", Value::Bool(true)),
        ("startsWith(0.00001, '1E-')", Value::Bool(true)),
        ("endsWith(0.00001, '-05')", Value::Bool(true)),
        ("toJSON(0.00001)", Value::String("1E-05".into())),
    ];
    for (source, expected) in rows {
        assert_eq!(eval(source, &context), expected, "row {source}");
    }
}

#[test]
fn builtin_function_examples_and_output_based_laziness() {
    let context = ctx();
    let rows = [
        ("contains('Hello World', 'world')", Value::Bool(true)),
        (
            "contains(fromJSON('[\"foo\",\"bar\"]'), 'foo')",
            Value::Bool(true),
        ),
        ("startsWith('Hello world', 'He')", Value::Bool(true)),
        ("endsWith('Hello world', 'ld')", Value::Bool(true)),
        (
            "format('Hello {0} {1} {2}!', 'Mona', 'the', 'Octocat')",
            Value::String("Hello Mona the Octocat!".into()),
        ),
        (
            "format('{{Hello {0}!}}', 'World')",
            Value::String("{Hello World!}".into()),
        ),
        (
            "format('{0}', 'used', fromJSON('bad'))",
            Value::String("used".into()),
        ),
        (
            "join(fromJSON('[\"a\",\"b\",\"c\"]'))",
            Value::String("a,b,c".into()),
        ),
        (
            "join(fromJSON('[\"a\",\"b\"]'), ', ')",
            Value::String("a, b".into()),
        ),
        (
            "join(fromJSON('[\"only\"]'), fromJSON('bad'))",
            Value::String("only".into()),
        ),
        (
            "contains(fromJSON('{}'), fromJSON('bad'))",
            Value::Bool(false),
        ),
        (
            "contains(fromJSON('[]'), fromJSON('bad'))",
            Value::Bool(false),
        ),
        (
            "startsWith(fromJSON('[]'), fromJSON('bad'))",
            Value::Bool(false),
        ),
        (
            "endsWith(fromJSON('{}'), fromJSON('bad'))",
            Value::Bool(false),
        ),
        ("('primitive')[fromJSON('bad')]", Value::Null),
    ];
    for (source, expected) in rows {
        assert_eq!(eval(source, &context), expected, "row {source}");
    }

    for source in [
        "contains(fromJSON('[1]'), fromJSON('bad'))",
        "startsWith('primitive', fromJSON('bad'))",
        "endsWith('primitive', fromJSON('bad'))",
    ] {
        let expression = parse(source).unwrap();
        assert!(matches!(
            evaluate(&expression, &context),
            Err(EvalError::FromJson(_))
        ));
    }
}

#[test]
fn json_rows_include_utf16_surrogate_pairs() {
    let context = ctx();
    assert_eq!(
        eval(r#"fromJSON('"\uD83D\uDE00"')"#, &context),
        Value::String("😀".into())
    );
    assert_eq!(
        eval("fromJSON('{''unquoted'': [1, 2,],}')", &context),
        Value::object(vec![(
            "unquoted".into(),
            Value::array(vec![Value::Number(1.0), Value::Number(2.0)]),
        )])
    );
    assert_eq!(
        eval("toJSON('hi')", &context),
        Value::String("\"hi\"".into())
    );

    let expression = parse(r#"fromJSON('"\uD83D"')"#).unwrap();
    assert!(matches!(
        evaluate(&expression, &context),
        Err(EvalError::FromJson(_))
    ));
}

#[test]
fn case_function_documented_rows_and_short_circuiting() {
    let main = ctx().with_github(Value::object(vec![(
        "ref".into(),
        Value::String("refs/heads/main".into()),
    )]));
    let feature = ctx().with_github(Value::object(vec![(
        "ref".into(),
        Value::String("refs/heads/feature/topic".into()),
    )]));
    let expression = "case(github.ref == 'refs/heads/main', 'production', github.ref == 'refs/heads/staging', 'staging', startsWith(github.ref, 'refs/heads/feature/'), 'development', 'unknown')";
    assert_eq!(eval(expression, &main), Value::String("production".into()));
    assert_eq!(
        eval(expression, &feature),
        Value::String("development".into())
    );
    assert_eq!(
        eval(
            "case(false, fromJSON('bad'), true, 'matched', fromJSON('also bad'))",
            &ctx()
        ),
        Value::String("matched".into())
    );
    assert_eq!(
        eval("CASE(false, 'no', 'default')", &ctx()),
        Value::String("default".into())
    );

    let non_boolean = parse("case(1, 'value', 'default')").unwrap();
    assert!(matches!(
        evaluate(&non_boolean, &ctx()),
        Err(EvalError::InvalidCasePredicate { .. })
    ));
    assert!(matches!(
        parse("case(true, 'a', false, 'b')"),
        Err(ParseError::EvenCaseParameters)
    ));
}

#[test]
fn status_functions_and_status_call_detection() {
    let succeeding = ctx().with_status(RunStatus::Success);
    let failing = ctx().with_status(RunStatus::Failure);
    let cancelling = ctx().with_status(RunStatus::Cancelled);
    assert_eq!(eval("success()", &succeeding), Value::Bool(true));
    assert_eq!(eval("failure()", &succeeding), Value::Bool(false));
    assert_eq!(eval("failure()", &failing), Value::Bool(true));
    assert_eq!(eval("cancelled()", &cancelling), Value::Bool(true));
    assert_eq!(eval("always()", &cancelling), Value::Bool(true));

    assert!(!expr_calls_status_function(
        &parse("github.ref == 'main'").unwrap()
    ));
    assert!(expr_calls_status_function(
        &parse("!cancelled() && github.ref == 'main'").unwrap()
    ));
}
