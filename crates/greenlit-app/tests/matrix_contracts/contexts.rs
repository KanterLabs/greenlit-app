//! Runtime context shapes and deferred matrix templates.

use super::common::*;
use super::support;

#[test]
fn context_shapes_distinguish_static_absence_from_runtime_slots() {
    let plan = plan_json(CONTEXT_SHAPES);

    let observer = job(&plan, "observer");
    assert_eq!(observer["env"]["NO_NEEDS"]["evaluation"], "static");
    assert_eq!(observer["env"]["NO_NEEDS"]["value"], "");

    let consumer = job(&plan, "consumer");
    assert_eq!(consumer["env"]["DIRECT"]["evaluation"], "deferred");
    assert_eq!(
        consumer["env"]["DIRECT"]["defers_on"],
        serde_json::json!([{
            "kind": "needs-output",
            "job": "producer",
            "output": "declared"
        }])
    );
    assert_eq!(consumer["env"]["RESULT"]["evaluation"], "deferred");
    assert_eq!(
        consumer["env"]["RESULT"]["defers_on"],
        serde_json::json!([{"kind": "needs-result", "job": "producer"}])
    );
    assert_eq!(consumer["env"]["RESULT_INDEXED"]["evaluation"], "deferred");
    assert_eq!(
        consumer["env"]["RESULT_INDEXED"]["defers_on"],
        serde_json::json!([{"kind": "needs-result", "job": "producer"}])
    );
    assert_eq!(
        consumer["env"]["RESULT_RUNTIME_INDEX"]["evaluation"],
        "deferred"
    );
    assert_eq!(
        consumer["env"]["RESULT_RUNTIME_INDEX"]["defers_on"],
        serde_json::json!([{"kind": "needs-result", "job": "producer"}])
    );
    for field in ["UNDECLARED", "NON_DIRECT"] {
        assert_eq!(consumer["env"][field]["evaluation"], "static");
        assert_eq!(consumer["env"][field]["value"], "");
    }

    let first = &consumer["steps"][0]["kind"]["script"];
    assert_eq!(first["evaluation"], "static");
    assert_eq!(
        first["value"],
        "echo current= future= missing= skipped-key="
    );
    let second = &consumer["steps"][1]["kind"]["script"];
    assert_eq!(second["evaluation"], "deferred");
    assert_eq!(
        second["defers_on"],
        serde_json::json!([{
            "kind": "step-output",
            "step": "first",
            "output": "value"
        }])
    );

    let outputs = &consumer["outputs"]["entries"];
    assert_eq!(outputs["first_value"]["evaluation"], "deferred");
    assert_eq!(
        outputs["first_value"]["defers_on"],
        serde_json::json!([{
            "kind": "step-output",
            "step": "first",
            "output": "value"
        }])
    );
    assert_eq!(outputs["first_whole"]["evaluation"], "deferred");
    assert_eq!(
        outputs["first_whole"]["defers_on"],
        serde_json::json!([
            {"kind": "step-output", "step": "first"},
            {"kind": "step-status", "step": "first", "field": "outcome"},
            {"kind": "step-status", "step": "first", "field": "conclusion"}
        ])
    );
    assert_eq!(outputs["second_result"]["evaluation"], "deferred");
    assert_eq!(
        outputs["second_result"]["defers_on"],
        serde_json::json!([{
            "kind": "step-status",
            "step": "second",
            "field": "conclusion"
        }])
    );
    assert_eq!(outputs["missing_value"]["evaluation"], "static");
    assert_eq!(outputs["missing_value"]["value"], "");

    let strategy = job(&plan, "strategy_shape");
    assert_eq!(strategy["strategy"]["fail_fast"]["evaluation"], "deferred");
    for leg in strategy["legs"].as_array().expect("strategy legs") {
        assert_eq!(leg["steps"][0]["kind"]["script"]["evaluation"], "static");
        assert_eq!(leg["steps"][0]["kind"]["script"]["value"], "echo total=2");
        assert_eq!(leg["steps"][1]["kind"]["script"]["evaluation"], "deferred");
        assert_eq!(
            leg["steps"][1]["kind"]["script"]["defers_on"],
            serde_json::json!([{"kind": "strategy-context"}])
        );
    }

    let undeclared_lints = plan["lints"]
        .as_array()
        .expect("lint array")
        .iter()
        .filter(|lint| lint["kind"] == "undeclared-needed-output")
        .collect::<Vec<_>>();
    assert_eq!(undeclared_lints.len(), 1);
    assert!(
        undeclared_lints[0]["message"]
            .as_str()
            .expect("lint message")
            .contains("missing")
    );
}

#[test]
fn needs_output_matrices_and_their_job_templates_remain_explicitly_deferred() {
    let plan = plan_json(DEFERRED_MATRICES);

    let whole = job(&plan, "whole");
    assert_eq!(whole["name_is_default"], false);
    assert_eq!(whole["strategy"]["matrix"]["evaluation"], "deferred");
    let whole_matrix = &whole["strategy"]["matrix"]["expressions"][0];
    assert_eq!(
        whole_matrix["source"],
        "${{ fromJSON(needs.producer.outputs.matrix) }}"
    );
    assert_eq!(
        whole_matrix["residual"],
        "fromJSON(needs.producer.outputs.matrix)"
    );
    assert_eq!(whole_matrix["path"], "matrix");
    assert_eq!(
        whole_matrix["defers_on"],
        serde_json::json!([{
            "kind": "needs-output",
            "job": "producer",
            "output": "matrix"
        }])
    );
    assert_eq!(whole["legs"], serde_json::json!([]));
    assert_eq!(whole["runner"]["evaluation"], "deferred");
    assert_eq!(
        whole["runner"]["defers_on"],
        serde_json::json!([{"kind": "matrix-context"}])
    );
    assert_eq!(whole["name"]["evaluation"], "deferred");
    assert_eq!(
        whole["name"]["defers_on"],
        serde_json::json!([
            {"kind": "matrix-context"},
            {"kind": "strategy-context"}
        ])
    );
    assert_eq!(whole["strategy"]["fail_fast"]["evaluation"], "deferred");
    assert_eq!(whole["strategy"]["max_parallel"]["evaluation"], "deferred");
    assert_eq!(
        whole["steps"][0]["kind"]["script"]["evaluation"],
        "deferred"
    );

    let inline = job(&plan, "inline");
    assert_eq!(inline["name_is_default"], true);
    assert_eq!(inline["skip"]["kind"], "need-skipped");
    assert_eq!(inline["skip"]["need"], "producer");
    assert_eq!(inline["strategy"]["matrix"]["evaluation"], "deferred");
    let inline_axis = &inline["strategy"]["matrix"]["expressions"][0];
    assert_eq!(
        inline_axis["source"],
        "${{ fromJSON(needs.producer.outputs.colors) }}"
    );
    assert_eq!(
        inline_axis["residual"],
        "fromJSON(needs.producer.outputs.colors)"
    );
    assert_eq!(inline_axis["path"], "matrix.color[0]");
    assert_eq!(
        inline_axis["defers_on"],
        serde_json::json!([{
            "kind": "needs-output",
            "job": "producer",
            "output": "colors"
        }])
    );
    assert_eq!(inline["legs"], serde_json::json!([]));
    assert_eq!(
        inline["strategy"]["matrix"]["expressions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        inline["steps"][0]["kind"]["script"]["evaluation"],
        "deferred"
    );
    assert_eq!(
        inline["steps"][0]["kind"]["script"]["defers_on"],
        serde_json::json!([
            {"kind": "matrix-context"},
            {"kind": "strategy-context"}
        ])
    );
    assert_eq!(
        inline["steps"][1]["kind"]["script"]["value"],
        "echo controls true 2"
    );

    let static_matrix = job(&plan, "static_matrix_deferred_control");
    assert_eq!(static_matrix["strategy"]["matrix"]["evaluation"], "static");
    assert_eq!(
        static_matrix["strategy"]["fail_fast"]["evaluation"],
        "deferred"
    );
    assert_eq!(static_matrix["legs"].as_array().unwrap().len(), 2);
    assert_eq!(static_matrix["legs"][0]["name"]["value"], "2-2");
    assert_eq!(static_matrix["legs"][1]["name"]["value"], "2-2");

    let after_inline = job(&plan, "after_inline");
    assert_eq!(after_inline["skip"]["kind"], "need-skipped");
    assert_eq!(after_inline["skip"]["need"], "inline");

    let missing_output_lints = plan["lints"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|lint| lint["kind"] == "undeclared-needed-output")
        .map(|lint| lint["message"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(missing_output_lints.len(), 3);
    assert!(
        missing_output_lints
            .iter()
            .any(|message| message.contains("missing_fail_fast"))
    );
    assert!(
        missing_output_lints
            .iter()
            .any(|message| message.contains("missing_max_parallel"))
    );
    assert!(
        missing_output_lints
            .iter()
            .any(|message| message.contains("missing_axis"))
    );

    let sandbox = sandbox_with_workflow(DEFERRED_MATRICES);
    let human = sandbox.run(&["plan", "-W", "matrix.yml"]);
    assert!(human.status.success());
    let stdout = support::stdout_text(&human);
    assert!(stdout.contains("strategy: matrix deferred"), "{stdout}");
    assert!(
        stdout.contains(
            "matrix: deferred <- fromJSON(needs.producer.outputs.matrix) (defers on: needs.producer.outputs.matrix)"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("runner template: deferred <- matrix.os (defers on: matrix.*)"),
        "{stdout}"
    );
}
