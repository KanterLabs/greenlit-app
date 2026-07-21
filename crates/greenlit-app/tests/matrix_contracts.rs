//! Public `litci plan --json` matrix contracts not covered by the checked-in
//! matrix-needs fixture: expression-produced axes and whole matrices,
//! include-only expansion, resolved strategy contexts, the exact 256-leg
//! boundary, and actionable shape/control/cap failures.

pub mod support;

use support::Sandbox;

const EXPRESSION_MATRICES: &str = r#"on: push
jobs:
  axis_expression:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        channel: ${{ fromJSON('["stable","beta"]') }}
    steps:
      - run: echo ${{ matrix.channel }}
  object_expression:
    runs-on: ubuntu-latest
    strategy:
      matrix: ${{ fromJSON('{"flavor":["debug","release"],"arch":["x64","arm64"]}') }}
    steps:
      - run: echo ${{ matrix.flavor }}-${{ matrix.arch }}
"#;

const INCLUDE_ONLY_STRATEGY: &str = r#"on: push
jobs:
  include_only:
    name: ${{ matrix.label }}-${{ strategy.job-index }}-${{ strategy.job-total }}-${{ strategy.fail-fast }}-${{ strategy.max-parallel }}
    runs-on: ubuntu-latest
    strategy:
      fail-fast: ${{ fromJSON('false') }}
      max-parallel: ${{ fromJSON('2') }}
      matrix:
        include:
          - label: first
          - label: second
    steps:
      - run: echo ${{ matrix.label }} ${{ strategy.job-index }} ${{ strategy.job-total }} ${{ strategy.fail-fast }} ${{ strategy.max-parallel }}
"#;

fn sandbox_with_workflow(source: &str) -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write("matrix.yml", source);
    sandbox.init_git();
    sandbox
}

fn plan_json(source: &str) -> serde_json::Value {
    let sandbox = sandbox_with_workflow(source);
    let output = sandbox.run(&["plan", "-W", "matrix.yml", "--json"]);
    assert!(
        output.status.success(),
        "plan failed: {}",
        support::stderr_text(&output)
    );
    serde_json::from_slice(&output.stdout).expect("plan stdout must be valid JSON")
}

fn job<'a>(plan: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    plan["jobs"]
        .as_array()
        .expect("jobs array")
        .iter()
        .find(|job| job["id"] == id)
        .unwrap_or_else(|| panic!("job '{id}' missing from plan"))
}

fn run_script(leg: &serde_json::Value) -> &str {
    leg["steps"][0]["kind"]["script"]["value"]
        .as_str()
        .expect("resolved run script")
}

fn whole_matrix_expression_workflow(matrix_json: &str) -> String {
    format!(
        "on: push\njobs:\n  matrix:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix: ${{{{ fromJSON('{matrix_json}') }}}}\n    steps:\n      - run: echo ${{{{ matrix.index }}}}\n"
    )
}

#[test]
fn expression_axis_arrays_and_whole_objects_expand_at_the_cli_boundary() {
    let plan = plan_json(EXPRESSION_MATRICES);

    let axis = job(&plan, "axis_expression");
    assert_eq!(axis["strategy"]["legs"].as_array().unwrap().len(), 2);
    assert_eq!(axis["strategy"]["legs"][0]["values"]["channel"], "stable");
    assert_eq!(axis["strategy"]["legs"][1]["values"]["channel"], "beta");
    assert_eq!(run_script(&axis["legs"][0]), "echo stable");
    assert_eq!(run_script(&axis["legs"][1]), "echo beta");

    let object = job(&plan, "object_expression");
    let legs = object["strategy"]["legs"]
        .as_array()
        .expect("whole-expression matrix legs");
    assert_eq!(legs.len(), 4);
    let combinations = legs
        .iter()
        .map(|leg| {
            (
                leg["values"]["flavor"].as_str().unwrap(),
                leg["values"]["arch"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        combinations,
        vec![
            ("debug", "x64"),
            ("debug", "arm64"),
            ("release", "x64"),
            ("release", "arm64"),
        ]
    );
    assert_eq!(run_script(&object["legs"][0]), "echo debug-x64");
    assert_eq!(run_script(&object["legs"][3]), "echo release-arm64");
}

#[test]
fn include_only_legs_resolve_strategy_controls_and_each_legs_context() {
    let plan = plan_json(INCLUDE_ONLY_STRATEGY);
    let matrix = job(&plan, "include_only");

    assert_eq!(matrix["strategy"]["is_matrix"], true);
    assert_eq!(matrix["strategy"]["fail_fast"], false);
    assert_eq!(matrix["strategy"]["max_parallel"], 2);
    assert_eq!(matrix["strategy"]["legs"].as_array().unwrap().len(), 2);
    assert_eq!(
        matrix["strategy"]["legs"][0]["origin"],
        serde_json::json!({"kind": "include", "entry_index": 0})
    );
    assert_eq!(
        matrix["strategy"]["legs"][1]["origin"],
        serde_json::json!({"kind": "include", "entry_index": 1})
    );

    assert_eq!(matrix["legs"][0]["name"]["value"], "first-0-2-false-2");
    assert_eq!(matrix["legs"][1]["name"]["value"], "second-1-2-false-2");
    assert_eq!(run_script(&matrix["legs"][0]), "echo first 0 2 false 2");
    assert_eq!(run_script(&matrix["legs"][1]), "echo second 1 2 false 2");
}

#[test]
fn exactly_256_legs_are_accepted_and_fully_serialized() {
    let values = (0..256).collect::<Vec<_>>();
    let matrix_json = serde_json::json!({"index": values}).to_string();
    let source = whole_matrix_expression_workflow(&matrix_json);
    let plan = plan_json(&source);
    let matrix = job(&plan, "matrix");

    assert_eq!(matrix["strategy"]["legs"].as_array().unwrap().len(), 256);
    assert_eq!(matrix["legs"].as_array().unwrap().len(), 256);
    assert_eq!(matrix["strategy"]["legs"][0]["index"], 0);
    assert_eq!(matrix["strategy"]["legs"][255]["index"], 255);
    assert_eq!(
        matrix["strategy"]["legs"][0]["values"]["index"].as_f64(),
        Some(0.0)
    );
    assert_eq!(
        matrix["strategy"]["legs"][255]["values"]["index"].as_f64(),
        Some(255.0)
    );
    assert_eq!(run_script(&matrix["legs"][0]), "echo 0");
    assert_eq!(run_script(&matrix["legs"][255]), "echo 255");
}

#[test]
fn malformed_matrix_shapes_controls_and_257_leg_paths_are_actionable() {
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
        assert!(
            output.stdout.is_empty(),
            "row '{name}' wrote JSON on failure"
        );
        let stderr = support::stderr_text(&output);
        assert!(stderr.contains(location), "row '{name}': {stderr}");
        assert!(stderr.contains(&message), "row '{name}': {stderr}");
        assert!(
            stderr.contains(
                "fix: fix the `strategy.matrix`/`include`/`exclude` entries per the message above"
            ),
            "row '{name}': {stderr}"
        );
    }
}
