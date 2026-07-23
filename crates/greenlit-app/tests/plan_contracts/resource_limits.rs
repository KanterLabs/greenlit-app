//! Aggregate template materialization limits at the real CLI boundary.

use super::common::*;
use super::support;

#[test]
fn mixed_template_memory_matches_runner_argument_and_brace_segments() {
    // FormatResultBuilder charges every nonempty appended segment separately,
    // while empty argument renderings cost nothing. The 250 nonempty values
    // consume (26 + 2*20,958)*250 = 10,485,500 bytes. For the accepted
    // literal, the escaped `{` segment costs 28 bytes and its 103-character
    // suffix costs 232, landing exactly on 10 MiB. Replacing one suffix byte
    // with `}` preserves the output length but splits another escaped-brace
    // segment, raising the counter by 26 bytes. Four empty expressions bring
    // the template to GitHub's 254-value `format()` limit without consuming
    // memory in the result builder.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/Expressions/Sdk/Functions/Format.cs#L214-L267
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/Expressions/Sdk/MemoryCounter.cs#L45-L57
    let default = "x".repeat(20_958);
    let repeated = "${{ inputs.chunk }}".repeat(250);
    let empty_arguments = "${{ '' }}".repeat(4);
    let workflow = |literal: &str| {
        format!(
            "on:\n  workflow_dispatch:\n    inputs:\n      chunk:\n        type: string\n        default: {default}\njobs:\n  bounded:\n    runs-on: ubuntu-latest\n    steps:\n      - run: \"{literal}{repeated}{empty_arguments}\"\n"
        )
    };

    let accepted_literal = format!("{{{}", "p".repeat(103));
    let accepted = sandbox_with_workflow(&workflow(&accepted_literal)).run(&[
        "plan",
        "-W",
        "contracts.yml",
        "-e",
        "workflow_dispatch",
    ]);
    assert!(
        accepted.status.success(),
        "exactly-at-limit mixed template failed: {}",
        support::stderr_text(&accepted)
    );

    let rejected_literal = format!("{{}}{}", "p".repeat(102));
    let sandbox = sandbox_with_workflow(&workflow(&rejected_literal));

    let output = sandbox.run(&[
        "plan",
        "-W",
        "contracts.yml",
        "-e",
        "workflow_dispatch",
        "--json",
    ]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("contracts.yml:11:14"), "{stderr}");
    assert!(
        stderr.contains("maximum allowed memory size of 10485760 bytes was exceeded"),
        "{stderr}"
    );
    assert!(
        stderr.contains("fix: fix the expression referenced above"),
        "{stderr}"
    );
    assert!(stderr.len() < 4_096, "diagnostic retained expanded data");
}
