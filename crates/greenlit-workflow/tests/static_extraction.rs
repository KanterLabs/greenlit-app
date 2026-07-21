//! Oracle table: `extract_static` — every `secrets.*`/`vars.*` reference,
//! whether a dynamic `vars[...]` lookup exists, every `uses:` reference,
//! and every `runs-on` value (`PHASE-1-engine-core.md` greenlit-workflow
//! section).

use greenlit_workflow::{extract_static, parse_workflow};

#[test]
fn finds_secrets_dot_and_bracket_references() {
    let source = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    env:\n      TOKEN: ${{ secrets.API_TOKEN }}\n    steps:\n      - run: echo ${{ secrets['DB_PASSWORD'] }}\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");
    let extraction = extract_static(&workflow);
    assert!(extraction.secrets.contains_key("API_TOKEN"));
    assert!(extraction.secrets.contains_key("DB_PASSWORD"));
    assert!(!extraction.secrets["API_TOKEN"].is_empty());
}

#[test]
fn finds_literal_vars_references_and_dynamic_lookups() {
    let source = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ vars.REGION }}\n      - run: echo ${{ vars[matrix.key] }}\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");
    let extraction = extract_static(&workflow);
    assert!(extraction.vars.contains_key("REGION"));
    assert!(extraction.has_dynamic_vars_lookup);
}

#[test]
fn no_dynamic_vars_lookup_when_only_literal_references_are_used() {
    let source = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ vars.REGION }}\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");
    let extraction = extract_static(&workflow);
    assert!(!extraction.has_dynamic_vars_lookup);
}

#[test]
fn if_condition_is_scanned_even_without_the_wrapper() {
    let source = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - if: secrets.FLAG == 'on'\n        run: echo hi\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");
    let extraction = extract_static(&workflow);
    assert!(extraction.secrets.contains_key("FLAG"));
}

#[test]
fn collects_every_uses_reference_in_document_order() {
    let source = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - uses: actions/setup-node@v4\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");
    let extraction = extract_static(&workflow);
    let refs: Vec<&str> = extraction.uses.iter().map(|u| u.value.as_str()).collect();
    assert_eq!(refs, ["actions/checkout@v4", "actions/setup-node@v4"]);
}

#[test]
fn collects_every_runs_on_value() {
    let source = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n  b:\n    runs-on: [self-hosted, linux]\n    steps:\n      - run: echo hi\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");
    let extraction = extract_static(&workflow);
    let values: Vec<&str> = extraction
        .runs_on
        .iter()
        .map(|r| r.value.as_str())
        .collect();
    assert_eq!(values, ["ubuntu-latest", "self-hosted", "linux"]);
}

#[test]
fn finds_references_inside_matrix_include_values_and_container_credentials() {
    let source = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        include:\n          - token: ${{ secrets.MATRIX_TOKEN }}\n    services:\n      db:\n        image: postgres\n        credentials:\n          password: ${{ secrets.DB_PASS }}\n    steps:\n      - run: echo hi\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");
    let extraction = extract_static(&workflow);
    assert!(extraction.secrets.contains_key("MATRIX_TOKEN"));
    assert!(extraction.secrets.contains_key("DB_PASS"));
}

#[test]
fn quoted_closing_delimiters_do_not_truncate_expression_scanning() {
    // GitHub expression strings use single quotes and escape a quote with
    // `''`; `}}` inside such a string is data, not the wrapper terminator:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#literals
    let source = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: |\n          echo \"${{ format('it''s }}-{0}', secrets.ESCAPED_TOKEN) }}\"\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");
    let extraction = extract_static(&workflow);
    assert!(extraction.secrets.contains_key("ESCAPED_TOKEN"));
}

#[test]
fn finds_vars_references_in_job_name() {
    // The context-availability table explicitly permits `vars` in
    // `jobs.<job_id>.name`:
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability
    let source = "on: push\njobs:\n  build:\n    name: Build ${{ vars.JOB_LABEL }}\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";
    let workflow = parse_workflow("ci.yml", source).expect("parses");
    let extraction = extract_static(&workflow);
    assert!(extraction.vars.contains_key("JOB_LABEL"));
}
