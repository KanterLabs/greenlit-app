//! Oracle boundaries for the runner's mixed-scalar `format()` synthesis.

use greenlit_expr::error::MAX_EXPRESSION_LENGTH;
use greenlit_workflow::{ParseError, parse_workflow};

fn workflow_with_run_template(template: &str) -> String {
    format!(
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: {template}\n"
    )
}

fn expression_error(source: &str) -> (String, String) {
    match parse_workflow("template.yml", source) {
        Err(ParseError::Expression {
            span,
            context,
            message,
        }) => {
            assert_eq!(span.start.line, 6);
            assert_eq!(span.start.column, 14);
            (context, message)
        }
        result => panic!("expected a mixed-template expression error, got {result:?}"),
    }
}

#[test]
fn mixed_template_placeholder_count_follows_the_runner_format_arity() {
    // TemplateReader assigns every authored expression segment its own
    // argument in one synthetic `format()` expression. The well-known
    // function accepts 255 total parameters: one pattern plus 254 values.
    // The older templater parses this synthetic expression when evaluating
    // the token; WorkflowParser validates it immediately. The acceptance
    // boundary is identical.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTObjectTemplating/ObjectTemplating/TemplateReader.cs#L503-L619
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/ObjectTemplating/TemplateReader.cs#L505-L627
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/Expressions/ExpressionConstants.cs#L11-L20
    let template = |count| "${{ github.missing }}".repeat(count);

    parse_workflow(
        "template.yml",
        &workflow_with_run_template("\"echo it's {literal} ${{ github.ref }}\""),
    )
    .unwrap_or_else(|error| {
        panic!("runner literal quote/brace escaping must produce a valid format call: {error}")
    });

    parse_workflow("template.yml", &workflow_with_run_template(&template(254)))
        .unwrap_or_else(|error| panic!("254 placeholders must remain valid: {error}"));

    let (context, message) = expression_error(&workflow_with_run_template(&template(255)));
    assert_eq!(context, "jobs.build.steps[0].run");
    assert!(message.contains("format"), "{message}");
    assert!(message.contains("1-255"), "{message}");
    assert!(message.contains("256"), "{message}");
}

#[test]
fn synthetic_format_expression_obeys_runner_length_and_depth_limits() {
    // The runner validates the complete generated expression, not only each
    // authored placeholder. Pin both aggregate boundaries so a refactor
    // cannot accidentally validate the segments in isolation again.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/Expressions/ExpressionConstants.cs#L29-L33
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/Expressions/ExpressionParser.cs#L402-L446
    let wrapper = "format('prefix-{0}', '')";
    let wrapper_units = wrapper.encode_utf16().count();
    let accepted_payload = "x".repeat(MAX_EXPRESSION_LENGTH - wrapper_units);
    let template = |payload: &str| format!("prefix-${{{{ '{payload}' }}}}");

    parse_workflow(
        "template.yml",
        &workflow_with_run_template(&template(&accepted_payload)),
    )
    .unwrap_or_else(|error| panic!("exact aggregate length limit must be accepted: {error}"));

    let rejected_payload = format!("{accepted_payload}x");
    let (_, message) = expression_error(&workflow_with_run_template(&template(&rejected_payload)));
    assert!(
        message.contains("expression is 21001 UTF-16 code units"),
        "{message}"
    );

    let nested = |not_count| format!("prefix-${{{{ {}true }}}}", "!".repeat(not_count));
    parse_workflow("template.yml", &workflow_with_run_template(&nested(48)))
        .unwrap_or_else(|error| panic!("aggregate AST depth 50 must be accepted: {error}"));

    let (_, message) = expression_error(&workflow_with_run_template(&nested(49)));
    assert!(message.contains("nesting exceeds"), "{message}");
    assert!(message.contains("50"), "{message}");
}
