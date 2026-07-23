//! Actionable matrix shape, control, and expansion-limit failures.

use super::common::*;
use super::support;

#[test]
fn anchored_matrix_amplification_hits_the_local_plan_size_limit() {
    // The workflow source and parsed YAML stay small: one anchored 128 KiB
    // scalar is crossed with 256 tiny indices. GitHub dispatches those legs
    // independently, but Greenlit retains one local plan and must reject at
    // its 64 MiB stable-JSON ceiling instead of exhausting process memory.
    let large_value = format!("plan-size-sentinel-{}", "x".repeat(128 * 1_024));
    let indices = (0..256)
        .map(|index| index.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        "on: push\njobs:\n  matrix:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        payload: [&large {large_value}]\n        index: [{indices}]\n    steps:\n      - run: echo bounded\n"
    );
    let sandbox = sandbox_with_workflow(&source);

    let output = sandbox.run(&["plan", "-W", "matrix.yml", "--json"]);
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("matrix.yml:3:3"), "{stderr}");
    assert!(
        stderr
            .contains("expanded execution plan exceeds Greenlit's 67108864-byte local size limit"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "fix: reduce expanded matrix data or context-expanded field sizes, then retry"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("plan-size-sentinel"), "{stderr}");
    assert!(stderr.len() < 4_096, "diagnostic retained matrix data");
}

#[test]
fn matrix_shape_control_cap_and_deferred_lower_bound_contracts_are_actionable() {
    let zero_axes = sandbox_with_workflow(
        "on: push\njobs:\n  producer:\n    runs-on: ubuntu-latest\n    outputs:\n      marker: ${{ steps.out.outputs.marker }}\n    steps:\n      - id: out\n        run: echo produce\n  matrix:\n    needs: producer\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        include:\n          - marker: ${{ needs.producer.outputs.marker }}\n    steps:\n      - run: echo deferred\n",
    );
    let output = zero_axes.run(&["plan", "-W", "matrix.yml", "--json"]);
    assert!(output.status.success(), "{}", support::stderr_text(&output));
    let plan: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("zero-axis deferred plan JSON");
    assert_eq!(
        job(&plan, "matrix")["strategy"]["matrix"]["evaluation"],
        "deferred"
    );

    let expression_rows = [
        (
            "non-object whole matrix",
            "[1,2]",
            "strategy.matrix expression must evaluate to an object",
        ),
        (
            "non-array axis",
            r#"{"axis":"one"}"#,
            "strategy.matrix expression field 'axis' must evaluate to an array, got string",
        ),
        (
            "non-array include",
            r#"{"include":{}}"#,
            "strategy.matrix expression field 'include' must evaluate to an array, got object",
        ),
        (
            "non-array exclude",
            r#"{"exclude":false}"#,
            "strategy.matrix expression field 'exclude' must evaluate to an array, got boolean",
        ),
        (
            "primitive include entry",
            r#"{"include":[1]}"#,
            "strategy.matrix.include[0] must evaluate to an object, got number",
        ),
        (
            "primitive exclude entry",
            r#"{"exclude":["nope"]}"#,
            "strategy.matrix.exclude[0] must evaluate to an object, got string",
        ),
    ];

    let mut rows = expression_rows
        .into_iter()
        .map(|(name, json, message)| {
            (
                name,
                whole_matrix_expression_workflow(json),
                "matrix.yml:6:15",
                message.to_string(),
            )
        })
        .collect::<Vec<_>>();

    rows.push((
        "non-array inline expression axis",
        "on: push\njobs:\n  matrix:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        channel: ${{ 'one' }}\n    steps:\n      - run: echo invalid\n"
            .to_string(),
        "matrix.yml:7:18",
        "strategy.matrix expression field 'channel' must evaluate to an array, got string"
            .to_string(),
    ));

    rows.push((
        "known-invalid axis beside a deferred axis",
        "on: push\njobs:\n  producer:\n    runs-on: ubuntu-latest\n    outputs:\n      values: ${{ steps.out.outputs.values }}\n    steps:\n      - id: out\n        run: echo produce\n  matrix:\n    needs: producer\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        dynamic: ${{ fromJSON(needs.producer.outputs.values) }}\n        bad: ${{ 'one' }}\n    steps:\n      - run: echo invalid\n"
            .to_string(),
        "matrix.yml:16:14",
        "strategy.matrix expression field 'bad' must evaluate to an array, got string"
            .to_string(),
    ));

    rows.push((
        "known-empty exclude beside a deferred axis",
        "on: push\njobs:\n  producer:\n    runs-on: ubuntu-latest\n    outputs:\n      values: ${{ steps.out.outputs.values }}\n    steps:\n      - id: out\n        run: echo produce\n  matrix:\n    needs: producer\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        dynamic: ${{ fromJSON(needs.producer.outputs.values) }}\n        exclude:\n          - {}\n    steps:\n      - run: echo invalid\n"
            .to_string(),
        "matrix.yml:17:13",
        "an `exclude` entry must name at least one matrix key".to_string(),
    ));

    let known_values = serde_json::to_string(&(0..257).collect::<Vec<_>>())
        .expect("serialize static matrix values");
    rows.push((
        "provably excessive static expression axis beside a deferred include",
        format!(
            "on: push\njobs:\n  producer:\n    runs-on: ubuntu-latest\n    outputs:\n      marker: ${{{{ steps.out.outputs.marker }}}}\n    steps:\n      - id: out\n        run: echo produce\n  matrix:\n    needs: producer\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        known: ${{{{ fromJSON('{known_values}') }}}}\n        include:\n          - marker: ${{{{ needs.producer.outputs.marker }}}}\n    steps:\n      - run: echo invalid\n"
        ),
        "matrix.yml:15:9",
        "matrix expands to 257 jobs, exceeding the limit of 256".to_string(),
    ));

    let product_values = (0..257).collect::<Vec<_>>();
    rows.push((
        "257-leg product",
        whole_matrix_expression_workflow(&serde_json::json!({"index": product_values}).to_string()),
        "matrix.yml:6:15",
        "matrix expands to 257 jobs, exceeding the limit of 256".to_string(),
    ));
    let include_values = (0..257)
        .map(|index| serde_json::json!({"index": index}))
        .collect::<Vec<_>>();
    rows.push((
        "257 include-created legs",
        whole_matrix_expression_workflow(
            &serde_json::json!({"include": include_values}).to_string(),
        ),
        "matrix.yml:6:15",
        "matrix expands to 257 jobs, exceeding the limit of 256".to_string(),
    ));

    rows.extend([
        (
            "non-boolean fail-fast",
            "on: push\njobs:\n  matrix:\n    runs-on: ubuntu-latest\n    strategy:\n      fail-fast: ${{ 'false' }}\n      matrix:\n        index: [0]\n    steps:\n      - run: echo invalid\n"
                .to_string(),
            "matrix.yml:6:18",
            "strategy.fail-fast must evaluate to a boolean, got string".to_string(),
        ),
        (
            "non-number max-parallel",
            "on: push\njobs:\n  matrix:\n    runs-on: ubuntu-latest\n    strategy:\n      max-parallel: ${{ '2' }}\n      matrix:\n        index: [0]\n    steps:\n      - run: echo invalid\n"
                .to_string(),
            "matrix.yml:6:21",
            "strategy.max-parallel must evaluate to a number, got string".to_string(),
        ),
        (
            "zero max-parallel",
            "on: push\njobs:\n  matrix:\n    runs-on: ubuntu-latest\n    strategy:\n      max-parallel: 0\n      matrix:\n        index: [0]\n    steps:\n      - run: echo invalid\n"
                .to_string(),
            "matrix.yml:6:21",
            "strategy.max-parallel must be a positive integer no greater than 4294967295, got 0"
                .to_string(),
        ),
        (
            "fractional max-parallel",
            "on: push\njobs:\n  matrix:\n    runs-on: ubuntu-latest\n    strategy:\n      max-parallel: 1.5\n      matrix:\n        index: [0]\n    steps:\n      - run: echo invalid\n"
                .to_string(),
            "matrix.yml:6:21",
            "strategy.max-parallel must be a positive integer no greater than 4294967295, got 1.5"
                .to_string(),
        ),
    ]);

    for (name, source, location, message) in rows {
        let sandbox = sandbox_with_workflow(&source);
        let output = sandbox.run(&["plan", "-W", "matrix.yml", "--json"]);
        assert!(!output.status.success(), "row '{name}' must fail");
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(location), "row '{name}': {stderr}");
        assert!(stderr.contains(&message), "row '{name}': {stderr}");
        assert!(
            stderr.contains("fix: fix the `strategy` field named in the message above"),
            "row '{name}': {stderr}"
        );
    }
}
