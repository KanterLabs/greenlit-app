//! Oracle tables for GitHub's workflow-template delimiter rules and the
//! per-key context/special-function availability table.

use greenlit_workflow::{ParseError, parse_workflow};

const QUOTE_AWARE_EXPRESSION: &str = "${{ format('it''s }} {0}', vars.VALUE) }}";
const UNCLOSED_EXPRESSION: &str = "${{ vars.VALUE";

struct TemplateRow {
    name: &'static str,
    source: &'static str,
}

#[test]
fn every_modeled_expression_site_uses_quote_aware_delimiters_and_reports_unclosed_templates() {
    // GitHub's TemplateReader ignores `}}` inside single-quoted expression
    // strings, including doubled-quote escapes:
    // https://github.com/actions/runner/blob/main/src/Sdk/DTObjectTemplating/ObjectTemplating/TemplateReader.cs
    let rows = [
        TemplateRow {
            name: "workflow env",
            source: "on: push\nenv:\n  VALUE: __EXPR__\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "job name",
            source: "on: push\njobs:\n  build:\n    name: __EXPR__\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "runs-on label",
            source: "on: push\njobs:\n  build:\n    runs-on: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "runs-on labels",
            source: "on: push\njobs:\n  build:\n    runs-on: [\"__EXPR__\"]\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "runs-on group",
            source: "on: push\njobs:\n  build:\n    runs-on:\n      group: __EXPR__\n      labels: [ubuntu-latest]\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "job if",
            source: "on: push\njobs:\n  build:\n    if: __EXPR__\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "job output",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    outputs:\n      value: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "job env",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    env:\n      VALUE: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "job default shell",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    defaults:\n      run:\n        shell: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "job default working directory",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    defaults:\n      run:\n        working-directory: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "whole matrix",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "matrix axis",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        value: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "matrix nested value",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        include:\n          - nested:\n              value: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "matrix exclude nested value",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        value: [one]\n        exclude:\n          - nested:\n              value: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "strategy fail-fast",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      fail-fast: __EXPR__\n      matrix:\n        value: [one]\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "strategy max-parallel",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      max-parallel: __EXPR__\n      matrix:\n        value: [one]\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "container shorthand",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "container credentials",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container:\n      image: alpine\n      credentials:\n        username: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "container env",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container:\n      image: alpine\n      env:\n        VALUE: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "container port",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container:\n      image: alpine\n      ports: [\"__EXPR__\"]\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "container volume",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container:\n      image: alpine\n      volumes: [\"__EXPR__\"]\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "container options",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container:\n      image: alpine\n      options: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "service container",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    services:\n      db:\n        image: __EXPR__\n    steps:\n      - run: echo hi\n",
        },
        TemplateRow {
            name: "step if",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - if: __EXPR__\n        run: echo hi\n",
        },
        TemplateRow {
            name: "step name",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - name: __EXPR__\n        run: echo hi\n",
        },
        TemplateRow {
            name: "step env",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - env:\n          VALUE: __EXPR__\n        run: echo hi\n",
        },
        TemplateRow {
            name: "step working directory",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - working-directory: __EXPR__\n        run: echo hi\n",
        },
        TemplateRow {
            name: "step continue-on-error",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - continue-on-error: __EXPR__\n        run: echo hi\n",
        },
        TemplateRow {
            name: "step timeout-minutes",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - timeout-minutes: __EXPR__\n        run: echo hi\n",
        },
        TemplateRow {
            name: "step run",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo \"__EXPR__\"\n",
        },
        TemplateRow {
            name: "step shell",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - shell: __EXPR__\n        run: echo hi\n",
        },
        TemplateRow {
            name: "step with",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          path: __EXPR__\n",
        },
    ];

    for row in rows {
        let valid = row.source.replace("__EXPR__", QUOTE_AWARE_EXPRESSION);
        parse_workflow("oracle.yml", &valid)
            .unwrap_or_else(|error| panic!("{} must accept quoted delimiters: {error}", row.name));

        let invalid = row.source.replace("__EXPR__", UNCLOSED_EXPRESSION);
        match parse_workflow("oracle.yml", &invalid) {
            Err(ParseError::Expression {
                span,
                context,
                message,
            }) => {
                assert_eq!(&*span.file, "oracle.yml", "row {}", row.name);
                assert!(!context.is_empty(), "row {}", row.name);
                assert!(
                    message.contains("not closed"),
                    "row {}: {message}",
                    row.name
                );
            }
            result => panic!("{} must reject an unclosed template: {result:?}", row.name),
        }
    }
}

struct PolicyRow {
    name: &'static str,
    source: &'static str,
    expected_context: &'static str,
    expected_message: &'static str,
}

#[test]
fn context_and_special_function_availability_is_validated_at_each_workflow_key() {
    // Rows are transcribed from GitHub's current context-availability table:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability
    let rejected = [
        PolicyRow {
            name: "needs in workflow env",
            source: "on: push\nenv:\n  VALUE: ${{ needs.build.result }}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            expected_context: "env",
            expected_message: "context 'needs'",
        },
        PolicyRow {
            name: "secrets in job if",
            source: "on: push\njobs:\n  build:\n    if: secrets.FLAG == 'yes'\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            expected_context: "jobs.build.if",
            expected_message: "context 'secrets'",
        },
        PolicyRow {
            name: "matrix while constructing strategy",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        value: [\"${{ matrix.value }}\"]\n    steps:\n      - run: echo hi\n",
            expected_context: "jobs.build.strategy",
            expected_message: "context 'matrix'",
        },
        PolicyRow {
            name: "secrets in runs-on",
            source: "on: push\njobs:\n  build:\n    runs-on: ${{ secrets.RUNNER }}\n    steps:\n      - run: echo hi\n",
            expected_context: "jobs.build.runs-on",
            expected_message: "context 'secrets'",
        },
        PolicyRow {
            name: "env in job env",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    env:\n      VALUE: ${{ env.OTHER }}\n    steps:\n      - run: echo hi\n",
            expected_context: "jobs.build.env",
            expected_message: "context 'env'",
        },
        PolicyRow {
            name: "secrets in job defaults",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    defaults:\n      run:\n        shell: ${{ secrets.SHELL }}\n    steps:\n      - run: echo hi\n",
            expected_context: "jobs.build.defaults.run",
            expected_message: "context 'secrets'",
        },
        PolicyRow {
            name: "hashFiles in job output",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    outputs:\n      value: ${{ hashFiles('**/*') }}\n    steps:\n      - run: echo hi\n",
            expected_context: "jobs.build.outputs",
            expected_message: "function 'hashFiles'",
        },
        PolicyRow {
            name: "secrets in container image",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container: ${{ secrets.IMAGE }}\n    steps:\n      - run: echo hi\n",
            expected_context: "jobs.build.container.image",
            expected_message: "context 'secrets'",
        },
        PolicyRow {
            name: "steps in container credentials",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container:\n      image: alpine\n      credentials:\n        password: ${{ steps.login.outputs.password }}\n    steps:\n      - run: echo hi\n",
            expected_context: "jobs.build.container.credentials",
            expected_message: "context 'steps'",
        },
        PolicyRow {
            name: "secrets in step if",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - if: ${{ secrets.FLAG }}\n        run: echo hi\n",
            expected_context: "jobs.build.steps[0].if",
            expected_message: "context 'secrets'",
        },
        PolicyRow {
            name: "status function outside if",
            source: "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - name: ${{ success() }}\n        run: echo hi\n",
            expected_context: "jobs.build.steps[0].name",
            expected_message: "function 'success'",
        },
        PolicyRow {
            name: "hashFiles at job if",
            source: "on: push\njobs:\n  build:\n    if: ${{ hashFiles('**/*') != '' }}\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            expected_context: "jobs.build.if",
            expected_message: "function 'hashFiles'",
        },
    ];

    for row in rejected {
        match parse_workflow("policy.yml", row.source) {
            Err(ParseError::Expression {
                span,
                context,
                message,
            }) => {
                assert_eq!(&*span.file, "policy.yml", "row {}", row.name);
                assert_eq!(context, row.expected_context, "row {}", row.name);
                assert!(
                    message.contains(row.expected_message),
                    "row {}: {message}",
                    row.name
                );
            }
            result => panic!("{} must fail site validation: {result:?}", row.name),
        }
    }

    let accepted = [
        (
            "secrets in workflow env",
            "on: push\nenv:\n  VALUE: ${{ secrets.VALUE }}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        ),
        (
            "status at job if",
            "on: push\njobs:\n  build:\n    if: ${{ !cancelled() && needs.setup.result == 'success' }}\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        ),
        (
            "matrix in job name",
            "on: push\njobs:\n  build:\n    name: Build ${{ matrix.os }}\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        os: [ubuntu-latest]\n    steps:\n      - run: echo hi\n",
        ),
        (
            "env in job defaults",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    defaults:\n      run:\n        working-directory: ${{ env.DIR }}\n    steps:\n      - run: echo hi\n",
        ),
        (
            "secrets in container credentials",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container:\n      image: alpine\n      credentials:\n        password: ${{ secrets.PASSWORD }}\n    steps:\n      - run: echo hi\n",
        ),
        (
            "hashFiles in step field",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - env:\n          HASH: ${{ hashFiles('**/*') }}\n        run: echo hi\n",
        ),
        (
            "status and hashFiles at step if",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - if: ${{ success() && hashFiles('**/*') != '' }}\n        run: echo hi\n",
        ),
    ];
    for (name, source) in accepted {
        parse_workflow("policy.yml", source)
            .unwrap_or_else(|error| panic!("{name} must be accepted: {error}"));
    }
}

#[test]
fn expression_errors_retain_the_modeled_field_span_and_inner_parser_detail() {
    let source = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github.ref == }}\n";
    match parse_workflow("syntax.yml", source) {
        Err(ParseError::Expression {
            span,
            context,
            message,
        }) => {
            assert_eq!(span.start.line, 6);
            assert_eq!(span.start.column, 14);
            assert_eq!(context, "jobs.build.steps[0].run");
            assert!(
                message.contains("unexpected end of expression"),
                "{message}"
            );
        }
        result => panic!("expected a span-bearing expression error, got {result:?}"),
    }
}

#[test]
fn expressions_are_rejected_at_documented_non_expression_sites() {
    let rows = [
        (
            "workflow default shell",
            "on: push\ndefaults:\n  run:\n    shell: ${{ vars.SHELL }}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            "defaults.run",
        ),
        (
            "workflow default working directory",
            "on: push\ndefaults:\n  run:\n    working-directory: ${{ vars.DIR }}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            "defaults.run",
        ),
        (
            "workflow name",
            "name: ${{ vars.NAME }}\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
            "name",
        ),
        (
            "step id",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - id: ${{ vars.ID }}\n        run: echo hi\n",
            "jobs.build.steps[0].id",
        ),
        (
            "step uses",
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: ${{ vars.ACTION }}\n",
            "jobs.build.steps[0].uses",
        ),
    ];
    for (name, source, expected_context) in rows {
        match parse_workflow("non-expression.yml", source) {
            Err(ParseError::Expression {
                context, message, ..
            }) => {
                assert_eq!(context, expected_context, "row {name}");
                assert!(message.contains("not allowed"), "row {name}: {message}");
            }
            result => panic!("{name} must reject expressions: {result:?}"),
        }
    }
}
