//! Oracle table: GitHub's YAML scalar-typing rules (design memo §6.1,
//! transcribing `actions/runner`'s `YamlObjectReader.cs`
//! `MatchNull`/`MatchBoolean`/`MatchInteger`/`MatchFloat`), driven through
//! the crate's real public API (`parse_workflow`) rather than the
//! `pub(crate)`-only matcher function directly — see
//! `src/yaml/scalar.rs`'s note on why. One row per documented spelling plus
//! the documented negative cases.

use greenlit_workflow::model::value::{ScalarOrExpr, YamlScalar};
use greenlit_workflow::{ParseError, parse_workflow};

fn workflow_with_env_value(raw_scalar: &str) -> String {
    format!(
        "on: push\nenv:\n  X: {raw_scalar}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    )
}

fn env_value(raw_scalar: &str) -> ScalarOrExpr {
    let source = workflow_with_env_value(raw_scalar);
    let workflow = parse_workflow("t.yml", &source)
        .unwrap_or_else(|e| panic!("expected {raw_scalar:?} to parse, got {e}"));
    workflow
        .env
        .into_iter()
        .next()
        .expect("exactly one env entry")
        .1
        .value
}

fn env_error(raw_scalar: &str) -> ParseError {
    let source = workflow_with_env_value(raw_scalar);
    parse_workflow("t.yml", &source).expect_err("expected a parse error")
}

#[test]
fn resolves_null_spellings() {
    for raw in ["null", "Null", "NULL", "~"] {
        assert_eq!(
            env_value(raw),
            ScalarOrExpr::Literal(YamlScalar::Null),
            "case {raw:?}"
        );
    }
}

#[test]
fn a_missing_mapping_value_resolves_to_null() {
    let source = "on: push\nenv:\n  X:\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";
    let workflow = parse_workflow("t.yml", source).expect("parses");
    assert_eq!(
        workflow.env[0].1.value,
        ScalarOrExpr::Literal(YamlScalar::Null)
    );
}

#[test]
fn resolves_boolean_spellings() {
    for raw in ["true", "True", "TRUE"] {
        assert_eq!(
            env_value(raw),
            ScalarOrExpr::Literal(YamlScalar::Bool(true)),
            "case {raw:?}"
        );
    }
    for raw in ["false", "False", "FALSE"] {
        assert_eq!(
            env_value(raw),
            ScalarOrExpr::Literal(YamlScalar::Bool(false)),
            "case {raw:?}"
        );
    }
}

#[test]
fn yes_no_on_off_are_strings_not_booleans() {
    // The documented reason `on:` works as a workflow mapping key.
    for raw in ["yes", "no", "on", "off", "y", "n", "Yes", "No"] {
        assert_eq!(
            env_value(raw),
            ScalarOrExpr::Literal(YamlScalar::String(raw.to_owned())),
            "case {raw:?}"
        );
    }
}

#[test]
fn resolves_decimal_integers() {
    let cases: &[(&str, f64)] = &[
        ("0", 0.0),
        ("123", 123.0),
        ("-123", -123.0),
        ("+123", 123.0),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            env_value(raw),
            ScalarOrExpr::Literal(YamlScalar::Number(*expected)),
            "case {raw:?}"
        );
    }
}

#[test]
fn resolves_hex_and_octal_integers_within_i32_range() {
    assert_eq!(
        env_value("0x1A"),
        ScalarOrExpr::Literal(YamlScalar::Number(26.0))
    );
    assert_eq!(
        env_value("0o17"),
        ScalarOrExpr::Literal(YamlScalar::Number(15.0))
    );
    assert_eq!(
        env_value("0x7FFFFFFF"),
        ScalarOrExpr::Literal(YamlScalar::Number(2_147_483_647.0))
    );
}

#[test]
fn only_unsigned_well_formed_radix_integer_literals_match() {
    // `actions/runner`'s `MatchInteger` checks `[+-][0-9]+` separately
    // from unsigned `0x...`/`0o...` branches:
    // https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ObjectTemplating/YamlObjectReader.cs
    for raw in ["-0x1", "+0x1", "-0o7", "+0o7", "0x", "0xGG", "0o", "0o8"] {
        assert_eq!(
            env_value(raw),
            ScalarOrExpr::Literal(YamlScalar::String(raw.to_owned())),
            "case {raw:?}"
        );
    }
}

#[test]
fn hex_and_octal_integer_overflow_is_a_parse_error_not_a_string() {
    // i32::MAX is 0x7FFFFFFF; one more hex digit overflows i32 range.
    let err = env_error("0x1FFFFFFFF");
    assert!(
        matches!(err, ParseError::IntegerOverflow { .. }),
        "expected IntegerOverflow, got {err:?}"
    );
    let err = env_error("0o37777777777");
    assert!(matches!(err, ParseError::IntegerOverflow { .. }));
}

#[test]
fn resolves_float_spellings() {
    let cases: &[(&str, f64)] = &[
        ("1.5", 1.5),
        ("-1.5", -1.5),
        (".5", 0.5),
        ("1.", 1.0),
        ("2e10", 2e10),
        ("2E10", 2e10),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            env_value(raw),
            ScalarOrExpr::Literal(YamlScalar::Number(*expected)),
            "case {raw:?}"
        );
    }
}

#[test]
fn out_of_range_decimal_number_is_a_parse_error() {
    // Once `MatchFloat` recognizes the grammar, the runner reports a failed
    // finite-range conversion as invalid rather than retaining infinity:
    // https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ObjectTemplating/YamlObjectReader.cs
    let err = env_error("1e9999");
    assert!(
        matches!(err, ParseError::Schema { ref message, .. } if message.contains("out of range")),
        "got {err:?}"
    );
}

#[test]
fn resolves_infinity_and_nan_spellings() {
    for raw in [".inf", "+.inf"] {
        match env_value(raw) {
            ScalarOrExpr::Literal(YamlScalar::Number(n)) => {
                assert_eq!(n, f64::INFINITY, "case {raw:?}")
            }
            other => panic!("case {raw:?}: expected +inf, got {other:?}"),
        }
    }
    match env_value("-.inf") {
        ScalarOrExpr::Literal(YamlScalar::Number(n)) => assert_eq!(n, f64::NEG_INFINITY),
        other => panic!("expected -inf, got {other:?}"),
    }
    for raw in [".nan", ".NaN", ".NAN"] {
        match env_value(raw) {
            ScalarOrExpr::Literal(YamlScalar::Number(n)) => assert!(n.is_nan(), "case {raw:?}"),
            other => panic!("case {raw:?}: expected NaN, got {other:?}"),
        }
    }
}

#[test]
fn two_dots_matches_neither_integer_nor_float_and_stays_a_string() {
    assert_eq!(
        env_value("1.2.3"),
        ScalarOrExpr::Literal(YamlScalar::String("1.2.3".to_owned()))
    );
}

#[test]
fn quoted_and_block_scalars_are_always_strings_regardless_of_content() {
    // Quoting/block style pins the value to string even when the content
    // would otherwise match Null/Boolean/Integer/Float.
    assert_eq!(
        env_value("'true'"),
        ScalarOrExpr::Literal(YamlScalar::String("true".to_owned()))
    );
    assert_eq!(
        env_value("\"123\""),
        ScalarOrExpr::Literal(YamlScalar::String("123".to_owned()))
    );
    assert_eq!(
        env_value("'~'"),
        ScalarOrExpr::Literal(YamlScalar::String("~".to_owned()))
    );
}

#[test]
fn explicit_core_tags_are_honored_and_override_style() {
    // `!!str 123` forces the string "123" even though `123` unquoted would
    // resolve as a Number.
    assert_eq!(
        env_value("!!str 123"),
        ScalarOrExpr::Literal(YamlScalar::String("123".to_owned()))
    );
    // `!!int` on a quoted scalar still forces Integer, overriding the
    // quoting-implies-string default.
    assert_eq!(
        env_value("!!int \"42\""),
        ScalarOrExpr::Literal(YamlScalar::Number(42.0))
    );
    assert_eq!(
        env_value("!!bool \"true\""),
        ScalarOrExpr::Literal(YamlScalar::Bool(true))
    );
    assert_eq!(
        env_value("!!float \"1\""),
        ScalarOrExpr::Literal(YamlScalar::Number(1.0))
    );
    assert_eq!(
        env_value("!!null \"~\""),
        ScalarOrExpr::Literal(YamlScalar::Null)
    );
}

#[test]
fn explicit_core_tag_mismatch_is_a_parse_error() {
    let err = env_error("!!bool maybe");
    assert!(matches!(err, ParseError::TagMismatch { .. }), "got {err:?}");
    let err = env_error("!!int not-a-number");
    assert!(matches!(err, ParseError::TagMismatch { .. }), "got {err:?}");
}

#[test]
fn unknown_yaml_tags_are_a_parse_error() {
    let err = env_error("!!timestamp 2024-01-01");
    assert!(
        matches!(err, ParseError::UnsupportedTag { .. }),
        "got {err:?}"
    );
    let err = env_error("!custom foo");
    assert!(
        matches!(err, ParseError::UnsupportedTag { .. }),
        "got {err:?}"
    );
}
