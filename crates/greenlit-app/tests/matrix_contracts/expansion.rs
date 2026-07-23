//! Static matrix expansion, types, strategy contexts, and the 256-leg boundary.

use super::common::*;

#[test]
fn matrix_values_expand_and_preserve_types_at_the_cli_boundary() {
    let plan = plan_json(EXPRESSION_MATRICES);

    let axis = job(&plan, "axis_expression");
    assert_eq!(
        axis["strategy"]["matrix"]["legs"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        axis["strategy"]["matrix"]["legs"][0]["values"]["channel"],
        serde_json::json!({"kind": "string", "value": "stable"})
    );
    assert_eq!(
        axis["strategy"]["matrix"]["legs"][1]["values"]["channel"],
        serde_json::json!({"kind": "string", "value": "beta"})
    );
    assert_eq!(run_script(&axis["legs"][0]), "echo stable");
    assert_eq!(run_script(&axis["legs"][1]), "echo beta");

    let object = job(&plan, "object_expression");
    let legs = object["strategy"]["matrix"]["legs"]
        .as_array()
        .expect("whole-expression matrix legs");
    assert_eq!(legs.len(), 4);
    let combinations = legs
        .iter()
        .map(|leg| {
            (
                leg["values"]["flavor"]["value"].as_str().unwrap(),
                leg["values"]["arch"]["value"].as_str().unwrap(),
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

    let tagged = job(&plan, "tagged_values");
    let values = tagged["strategy"]["matrix"]["legs"]
        .as_array()
        .expect("tagged value legs")
        .iter()
        .map(|leg| leg["values"]["value"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        serde_json::json!([
            {"kind": "null"},
            {"kind": "boolean", "value": false},
            {"kind": "number", "value": 1.5},
            {"kind": "string", "value": "text"},
            {"kind": "sequence", "value": [
                {"kind": "string", "value": "nested"}
            ]},
            {"kind": "mapping", "value": {
                "nested": {"kind": "boolean", "value": true}
            }},
            {"kind": "number", "value": "NaN"},
            {"kind": "number", "value": "Infinity"},
            {"kind": "number", "value": "-Infinity"}
        ])
        .as_array()
        .expect("expected tagged value array")
        .clone()
    );
}

#[test]
fn include_only_legs_resolve_strategy_controls_and_each_legs_context() {
    let plan = plan_json(INCLUDE_ONLY_STRATEGY);
    let matrix = job(&plan, "include_only");

    assert_eq!(matrix["strategy"]["matrix"]["evaluation"], "static");
    assert_eq!(matrix["strategy"]["fail_fast"], false);
    assert_eq!(matrix["strategy"]["max_parallel"], 2);
    assert_eq!(
        matrix["strategy"]["matrix"]["legs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        matrix["strategy"]["matrix"]["legs"][0]["origin"],
        serde_json::json!({"kind": "include", "entry_index": 0})
    );
    assert_eq!(
        matrix["strategy"]["matrix"]["legs"][1]["origin"],
        serde_json::json!({"kind": "include", "entry_index": 1})
    );

    assert_eq!(matrix["legs"][0]["name"]["value"], "first-0-2-false-2");
    assert_eq!(matrix["legs"][1]["name"]["value"], "second-1-2-false-2");
    assert_eq!(run_script(&matrix["legs"][0]), "echo first 0 2 false 2");
    assert_eq!(run_script(&matrix["legs"][1]), "echo second 1 2 false 2");
}

#[test]
fn omitted_max_parallel_defaults_to_job_total_in_each_strategy_context() {
    let plan = plan_json(DEFAULT_MAX_PARALLEL);
    let matrix = job(&plan, "default_parallel");

    assert!(matrix["strategy"]["max_parallel"].is_null());
    assert_eq!(matrix["legs"].as_array().unwrap().len(), 3);
    for leg in matrix["legs"].as_array().unwrap() {
        assert_eq!(leg["name"]["value"], "3-3");
        assert_eq!(run_script(leg), "echo 3");
    }
}

#[test]
fn exactly_256_legs_are_accepted_and_fully_serialized() {
    let values = (0..256).collect::<Vec<_>>();
    let matrix_json = serde_json::json!({"index": values}).to_string();
    let source = whole_matrix_expression_workflow(&matrix_json);
    let plan = plan_json(&source);
    let matrix = job(&plan, "matrix");

    assert_eq!(
        matrix["strategy"]["matrix"]["legs"]
            .as_array()
            .unwrap()
            .len(),
        256
    );
    assert_eq!(matrix["legs"].as_array().unwrap().len(), 256);
    assert_eq!(matrix["strategy"]["matrix"]["legs"][0]["index"], 0);
    assert_eq!(matrix["strategy"]["matrix"]["legs"][255]["index"], 255);
    assert_eq!(
        matrix["strategy"]["matrix"]["legs"][0]["values"]["index"]["value"].as_f64(),
        Some(0.0)
    );
    assert_eq!(
        matrix["strategy"]["matrix"]["legs"][255]["values"]["index"]["value"].as_f64(),
        Some(255.0)
    );
    assert_eq!(run_script(&matrix["legs"][0]), "echo 0");
    assert_eq!(run_script(&matrix["legs"][255]), "echo 255");
}
