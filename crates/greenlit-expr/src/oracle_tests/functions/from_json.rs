//! Public-boundary `fromJSON` oracle rows pinned to the legacy runner reader.

use crate::{EvalError, Value, evaluate, parse};

use super::{ctx, eval};

#[test]
fn matches_newtonsoft_default_depth_limit() {
    // `FromJson.cs` constructs a JsonTextReader without changing MaxDepth;
    // Newtonsoft's documented/default limit is 64 nested containers.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/FromJson.cs
    // https://github.com/JamesNK/Newtonsoft.Json/blob/13.0.3/Src/Newtonsoft.Json/JsonReader.cs
    let context = ctx();
    let depth_64 = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    let accepted = parse(&format!("fromJSON('{depth_64}')"))
        .unwrap_or_else(|error| panic!("depth-64 JSON expression did not parse: {error}"));
    assert!(evaluate(&accepted, &context).is_ok());

    let depth_65 = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    let rejected = parse(&format!("fromJSON('{depth_65}')"))
        .unwrap_or_else(|error| panic!("depth-65 JSON expression did not parse: {error}"));
    assert!(matches!(
        evaluate(&rejected, &context),
        Err(EvalError::FromJson(_))
    ));

    let constructor_64 = format!("{}0{}", "new F(".repeat(64), ")".repeat(64));
    let accepted = parse(&format!("fromJSON('{constructor_64}')"))
        .unwrap_or_else(|error| panic!("depth-64 constructor expression did not parse: {error}"));
    assert!(evaluate(&accepted, &context).is_ok());

    let constructor_65 = format!("{}0{}", "new F(".repeat(65), ")".repeat(65));
    let rejected = parse(&format!("fromJSON('{constructor_65}')"))
        .unwrap_or_else(|error| panic!("depth-65 constructor expression did not parse: {error}"));
    assert!(matches!(
        evaluate(&rejected, &context),
        Err(EvalError::FromJson(_))
    ));
}

#[test]
fn matches_newtonsoft_legacy_lexer_and_jtoken_conversion() {
    // The pinned runner leaves StrictJsonParsing off and uses Newtonsoft
    // JsonTextReader 13.0.3 with DateParseHandling.None and Double floats.
    // These rows pin its JavaScript extensions and JToken conversion boundary.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/FromJson.cs#L18-L24
    // https://github.com/JamesNK/Newtonsoft.Json/blob/13.0.3/Src/Newtonsoft.Json/JsonTextReader.cs#L1578-L1691
    // https://github.com/JamesNK/Newtonsoft.Json/blob/13.0.3/Src/Newtonsoft.Json/JsonTextReader.cs#L1963-L2237
    let context = ctx();
    let accepted = [
        ("fromJSON('0x10')", Value::Number(16.0)),
        ("fromJSON('0X10')", Value::Number(16.0)),
        ("fromJSON('010')", Value::Number(8.0)),
        ("fromJSON('-010')", Value::Number(-10.0)),
        ("fromJSON('0xffffffffffffffff')", Value::Number(-1.0)),
        ("fromJSON('1 true')", Value::Number(1.0)),
        ("fromJSON('{a:1} trailing').a", Value::Number(1.0)),
        (r#"fromJSON('{1a:2}')['1a']"#, Value::Number(2.0)),
        (r#"fromJSON('{é:3}')['é']"#, Value::Number(3.0)),
        (r#"fromJSON('{١a:4}')['١a']"#, Value::Number(4.0)),
        (
            "fromJSON('/*root comment*/1')",
            Value::String("root comment".into()),
        ),
        (
            "fromJSON('//root comment')",
            Value::String("root comment".into()),
        ),
        ("fromJSON('undefined')", Value::String(String::new())),
        (
            "fromJSON('[,,]')",
            Value::array(vec![
                Value::String(String::new()),
                Value::String(String::new()),
            ]),
        ),
        (
            r#"fromJSON('"\uD800"')"#,
            Value::String(char::REPLACEMENT_CHARACTER.into()),
        ),
        (
            r#"fromJSON('"\uD800\uD800\uDC00"')"#,
            Value::String(format!("{}𐀀", char::REPLACEMENT_CHARACTER)),
        ),
        (
            r#"fromJSON('"2024-01-02T03:04:05Z"')"#,
            Value::String("2024-01-02T03:04:05Z".into()),
        ),
        (
            "fromJSON('new Date(1)')",
            Value::String("new Date(\n  1\n)".into()),
        ),
        (
            "fromJSON('new Foo()')",
            Value::String("new Foo(\n)".into()),
        ),
        (
            "fromJSON('new Foo(1.0,1e2,0X10,010,undefined,{A:1,a:2,A:3},[,,])')",
            Value::String(
                "new Foo(\n  1.0,\n  100.0,\n  16,\n  8,\n  undefined,\n  {\n    \"A\": 3,\n    \"a\": 2\n  },\n  [\n    undefined,\n    undefined\n  ]\n)"
                    .into(),
            ),
        ),
        (
            r#"fromJSON('new Outer(/*ignored*/new Inner(true), "x")')"#,
            Value::String("new Outer(\n  new Inner(\n    true\n  ),\n  \"x\"\n)".into()),
        ),
        (
            "fromJSON('new Foo(1,)')",
            Value::String("new Foo(\n  1\n)".into()),
        ),
        (
            "fromJSON('new Foo(,)')",
            Value::String("new Foo(\n  undefined\n)".into()),
        ),
        (
            "fromJSON('new Float(1e16,1e17,1e-4,1e-5,1.2345678901234567)')",
            Value::String(
                "new Float(\n  10000000000000000.0,\n  1E+17,\n  0.0001,\n  1E-05,\n  1.2345678901234567\n)"
                    .into(),
            ),
        ),
    ];
    for (source, expected) in accepted {
        assert_eq!(eval(source, &context), expected, "accepted row {source}");
    }

    let rejected = [
        "fromJSON('+1')",
        "fromJSON('08')",
        "fromJSON('1abc')",
        "fromJSON('Infinity1')",
        "fromJSON('/*unterminated')",
        "fromJSON('//')",
        "fromJSON('{a/*comment*/:1}')",
        "fromJSON('{Ⅷ:5}')",
        "fromJSON('{𐀀:6}')",
        "fromJSON('true)')",
        "fromJSON('new ()')",
        "fromJSON('new Foo_bar(1)')",
    ];
    for source in rejected {
        let expression =
            parse(source).unwrap_or_else(|error| panic!("parse({source:?}) failed: {error}"));
        assert!(
            matches!(evaluate(&expression, &context), Err(EvalError::FromJson(_))),
            "rejected row {source}"
        );
    }
}

#[test]
fn matches_newtonsoft_numeric_boundaries_and_constructor_roundtrip_format() {
    // Differentially probed against Newtonsoft 13.0.3 on .NET 8.0. These
    // rows cover the radix sign boundary, Int64-to-BigInteger transition,
    // accepted legacy decimal spellings, overflow/underflow, and the
    // `Double.ToString("R")` formatting exposed by JConstructor.ToString().
    let context = ctx();
    let accepted = [
        ("fromJSON('.5')", Value::Number(0.5)),
        ("fromJSON('-.5')", Value::Number(-0.5)),
        ("fromJSON('1.')", Value::Number(1.0)),
        ("fromJSON('1.e2')", Value::Number(100.0)),
        ("fromJSON('1E+02')", Value::Number(100.0)),
        ("fromJSON('1e309')", Value::Number(f64::INFINITY)),
        ("fromJSON('1e-324')", Value::Number(0.0)),
        ("fromJSON('5e-324')", Value::Number(f64::from_bits(1))),
        (
            "fromJSON('0x8000000000000000')",
            Value::Number(-9_223_372_036_854_775_808.0),
        ),
        (
            "fromJSON('01000000000000000000000')",
            Value::Number(-9_223_372_036_854_775_808.0),
        ),
        (
            "fromJSON('new Ints(9223372036854775808,9007199254740993,00000000000000000000000001)')",
            Value::String(
                "new Ints(\n  9223372036854775808,\n  9007199254740993,\n  1\n)".into(),
            ),
        ),
        (
            "fromJSON('new N(-0.0,1e14,1e15,1e16,1e17,1e-3,1e-4,9.999999999999999e-5,1e-5,5e-324,1.7976931348623157e308,2.2250738585072014e-308,0.10000000000000002,9007199254740991.0,9007199254740992.0,9007199254740993.0,1e-99,1e-100)')",
            Value::String(
                "new N(\n  -0.0,\n  100000000000000.0,\n  1000000000000000.0,\n  10000000000000000.0,\n  1E+17,\n  0.001,\n  0.0001,\n  9.999999999999999E-05,\n  1E-05,\n  5E-324,\n  1.7976931348623157E+308,\n  2.2250738585072014E-308,\n  0.10000000000000002,\n  9007199254740991.0,\n  9007199254740992.0,\n  9007199254740992.0,\n  1E-99,\n  1E-100\n)"
                    .into(),
            ),
        ),
    ];
    for (source, expected) in accepted {
        assert_eq!(eval(source, &context), expected, "accepted row {source}");
    }

    let negative_big_integer_zero = format!("fromJSON('-{}')", "0".repeat(379));
    let Value::Number(negative_big_integer_zero) = eval(&negative_big_integer_zero, &context)
    else {
        panic!("380-character BigInteger zero did not convert to a number");
    };
    assert_eq!(negative_big_integer_zero.to_bits(), 0.0_f64.to_bits());

    for source in [
        "fromJSON('.')",
        "fromJSON('-.')",
        "fromJSON('1e')",
        "fromJSON('1e+')",
        "fromJSON('1e-')",
        "fromJSON('01e2')",
        "fromJSON('00.1')",
        "fromJSON('0x10000000000000000')",
        "fromJSON('02000000000000000000000')",
    ] {
        let expression =
            parse(source).unwrap_or_else(|error| panic!("parse({source:?}) failed: {error}"));
        assert!(
            matches!(evaluate(&expression, &context), Err(EvalError::FromJson(_))),
            "rejected row {source}"
        );
    }
}

#[test]
fn matches_newtonsoft_dotnet_8_whitespace_code_units() {
    // JsonTextReader delegates its whitespace predicate to
    // `char.IsWhiteSpace`. The runner's .NET 8 runtime uses exactly the
    // Unicode White_Space set below; adjacent historical/lookalike controls
    // remain syntax errors.
    // https://github.com/JamesNK/Newtonsoft.Json/blob/13.0.3/Src/Newtonsoft.Json/JsonTextReader.cs#L1835-L1868
    // https://github.com/dotnet/runtime/blob/v8.0.0/src/libraries/System.Private.CoreLib/src/System/Globalization/CharUnicodeInfo.cs#L947-L965
    let context = ctx();
    let accepted = [
        '\u{0009}', '\u{000A}', '\u{000B}', '\u{000C}', '\u{000D}', '\u{0020}', '\u{0085}',
        '\u{00A0}', '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}',
        '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}', '\u{2028}',
        '\u{2029}', '\u{202F}', '\u{205F}', '\u{3000}',
    ];
    for whitespace in accepted {
        let source = format!("fromJSON('{whitespace}true')");
        assert_eq!(
            eval(&source, &context),
            Value::Bool(true),
            "U+{:04X}",
            whitespace as u32
        );
    }

    for non_whitespace in [
        '\u{001C}', '\u{001D}', '\u{001E}', '\u{001F}', '\u{180E}', '\u{200B}', '\u{FEFF}',
    ] {
        let source = format!("fromJSON('{non_whitespace}true')");
        let expression = parse(&source)
            .unwrap_or_else(|error| panic!("parse(U+{:04X}): {error}", non_whitespace as u32));
        assert!(
            matches!(evaluate(&expression, &context), Err(EvalError::FromJson(_))),
            "U+{:04X}",
            non_whitespace as u32
        );
    }
}
