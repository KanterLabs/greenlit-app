//! Static preflight extraction through the public workflow API.

use greenlit_workflow::{extract_static, parse_workflow};

use super::HEADER;

#[test]
fn static_extraction_reports_the_complete_preflight_inventory() {
    let source = format!(
        "{HEADER}run-name: Deploy ${{{{ vars.RUN_LABEL }}}}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        include:\n          - token: ${{{{ vars.MATRIX_VALUE }}}}\n    container:\n      image: node:20\n      credentials:\n        password: ${{{{ secrets.REGISTRY_PASSWORD }}}}\n    services:\n      db:\n        image: postgres\n        credentials:\n          password: ${{{{ secrets.DB_PASS }}}}\n    env:\n      TOKEN: ${{{{ secrets.API_TOKEN }}}}\n      GH_TOKEN: ${{{{ github.token }}}}\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          region: ${{{{ vars.REGION }}}}\n      - run: echo ${{{{ secrets['DB_PASSWORD'] }}}} ${{{{ vars['DEPLOY_ENV'] }}}} ${{{{ vars[matrix.key] }}}}\n  package:\n    needs: build\n    runs-on: [self-hosted, linux]\n    steps:\n      - uses: actions/setup-node@v4\n      - run: echo ${{{{ needs.build.outputs.literal }}}} ${{{{ needs[format('{{0}}', 'build')].outputs[format('{{0}}', 'computed')] }}}} ${{{{ needs[github.event_name].outputs.dynamic }}}}\n"
    );
    let workflow = parse_workflow("inventory.yml", &source).expect("parses");
    let extraction = extract_static(&workflow).expect("valid expressions extract");

    let secret_names: Vec<&str> = extraction.secrets.keys().map(String::as_str).collect();
    assert_eq!(
        secret_names,
        ["API_TOKEN", "DB_PASS", "DB_PASSWORD", "REGISTRY_PASSWORD",]
    );
    let variable_names: Vec<&str> = extraction.vars.keys().map(String::as_str).collect();
    assert_eq!(
        variable_names,
        ["DEPLOY_ENV", "MATRIX_VALUE", "REGION", "RUN_LABEL"]
    );
    assert!(extraction.has_dynamic_vars_lookup);
    assert_eq!(extraction.dynamic_vars.len(), 1);
    assert!(extraction.references_github_token);

    let uses: Vec<&str> = extraction
        .uses
        .iter()
        .map(|reference| reference.value.as_str())
        .collect();
    assert_eq!(uses, ["actions/checkout@v4", "actions/setup-node@v4"]);
    let runner_labels: Vec<&str> = extraction
        .runs_on
        .iter()
        .map(|label| label.value.as_str())
        .collect();
    assert_eq!(runner_labels, ["ubuntu-latest", "self-hosted", "linux"]);
    let needs_references = extraction
        .needs_outputs
        .iter()
        .map(|reference| {
            (
                reference.referencing_job.as_str(),
                reference.referenced_job.as_str(),
                reference.output.as_str(),
                reference.span.file.as_ref(),
                reference.span.start.line,
            )
        })
        .collect::<Vec<_>>();
    let needs_line = u32::try_from(
        source
            .lines()
            .position(|line| line.contains("needs.build.outputs.literal"))
            .expect("needs reference line")
            + 1,
    )
    .expect("fixture line fits u32");
    assert_eq!(
        needs_references,
        [
            ("package", "build", "literal", "inventory.yml", needs_line),
            ("package", "build", "computed", "inventory.yml", needs_line),
        ]
    );

    let literal_only = format!(
        "{HEADER}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{{{ vars.REGION }}}}\n"
    );
    let workflow = parse_workflow("literal-only.yml", &literal_only).expect("parses");
    let extraction = extract_static(&workflow).expect("valid expressions extract");
    assert!(!extraction.has_dynamic_vars_lookup);
    assert!(!extraction.references_github_token);
}
