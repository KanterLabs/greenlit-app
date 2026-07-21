//! Oracle table: `strategy:` / `strategy.matrix:`, including `include`,
//! `exclude`, `fail-fast`, `max-parallel`, and the whole-value
//! `${{ fromJSON(...) }}` expression form (`PHASE-1-engine-core.md`
//! greenlit-workflow section; GitHub's
//! [matrix documentation](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/run-job-variations)).

use greenlit_workflow::model::job::MatrixSource;
use greenlit_workflow::model::value::{ScalarOrExpr, YamlScalar, YamlValue};
use greenlit_workflow::parse_workflow;

const HEADER: &str = "on: push\n";

#[test]
fn matrix_axes_include_exclude() {
    let source = format!(
        "{HEADER}jobs:\n  test:\n    runs-on: ubuntu-latest\n    strategy:\n      fail-fast: false\n      max-parallel: 4\n      matrix:\n        os: [ubuntu-latest, ubuntu-22.04]\n        node: [18, 20]\n        include:\n          - os: ubuntu-latest\n            node: 18\n            extra: true\n        exclude:\n          - os: ubuntu-22.04\n            node: 20\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    let strategy = &workflow.jobs[0].strategy.as_ref().unwrap().value;
    assert_eq!(
        strategy.fail_fast.as_ref().unwrap().value,
        ScalarOrExpr::Literal(YamlScalar::Bool(false))
    );
    assert_eq!(
        strategy.max_parallel.as_ref().unwrap().value,
        ScalarOrExpr::Literal(YamlScalar::Number(4.0))
    );
    let matrix = match &strategy.matrix.as_ref().unwrap().value {
        MatrixSource::Inline(m) => m,
        other => panic!("expected Inline matrix, got {other:?}"),
    };
    assert_eq!(matrix.axes[0].0.value, "os");
    assert_eq!(matrix.axes[1].0.value, "node");
    // Matrix axis values keep their YAML type: `node: [18, 20]` are
    // numbers, not strings, matching GitHub's typed matrix examples.
    assert_eq!(
        matrix.axes[1].1[0].value,
        YamlValue::Scalar(ScalarOrExpr::Literal(YamlScalar::Number(18.0)))
    );
    assert_eq!(matrix.include.len(), 1);
    let include_entry = &matrix.include[0].value;
    assert_eq!(include_entry[0].0.value, "os");
    assert_eq!(
        include_entry[2].1.value,
        YamlValue::Scalar(ScalarOrExpr::Literal(YamlScalar::Bool(true)))
    );
    assert_eq!(matrix.exclude.len(), 1);
    assert_eq!(matrix.exclude[0].value[0].0.value, "os");
}

#[test]
fn matrix_as_whole_value_expression() {
    let source = format!(
        "{HEADER}jobs:\n  test:\n    needs: setup\n    runs-on: ubuntu-latest\n    strategy:\n      matrix: ${{{{ fromJSON(needs.setup.outputs.matrix) }}}}\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    let strategy = &workflow.jobs[0].strategy.as_ref().unwrap().value;
    match &strategy.matrix.as_ref().unwrap().value {
        MatrixSource::Expression(expr) => {
            assert_eq!(expr.value, "${{ fromJSON(needs.setup.outputs.matrix) }}");
        }
        other => panic!("expected Expression matrix, got {other:?}"),
    }
}

#[test]
fn matrix_axis_can_be_an_expression_backed_array() {
    // GitHub's "Using contexts to create matrices" example assigns an
    // event-payload array expression directly to one matrix axis:
    // https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/run-job-variations#using-contexts-to-create-matrices
    let source = format!(
        "{HEADER}jobs:\n  test:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        version: ${{{{ github.event.client_payload.versions }}}}\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("expression-backed axis parses");
    let matrix = match &workflow.jobs[0]
        .strategy
        .as_ref()
        .expect("strategy")
        .value
        .matrix
        .as_ref()
        .expect("matrix")
        .value
    {
        MatrixSource::Inline(matrix) => matrix,
        other => panic!("expected Inline matrix, got {other:?}"),
    };
    assert_eq!(matrix.axes[0].0.value, "version");
    assert_eq!(matrix.axes[0].1.len(), 1);
    assert_eq!(
        matrix.axes[0].1[0].value,
        YamlValue::Scalar(ScalarOrExpr::Expression(
            "${{ github.event.client_payload.versions }}".to_owned()
        ))
    );
}

#[test]
fn matrix_include_supports_nested_structure() {
    let source = format!(
        "{HEADER}jobs:\n  test:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        include:\n          - os: ubuntu-latest\n            settings:\n              flags: [\"-Wall\", \"-Werror\"]\n    steps:\n      - run: echo hi\n"
    );
    let workflow = parse_workflow("t.yml", &source).expect("parses");
    let matrix = match &workflow.jobs[0]
        .strategy
        .as_ref()
        .unwrap()
        .value
        .matrix
        .as_ref()
        .unwrap()
        .value
    {
        MatrixSource::Inline(m) => m.clone(),
        other => panic!("expected Inline matrix, got {other:?}"),
    };
    let entry = &matrix.include[0].value;
    let (_, settings) = &entry[1];
    match &settings.value {
        YamlValue::Mapping(fields) => {
            let (_, flags) = &fields[0];
            match &flags.value {
                YamlValue::Sequence(items) => assert_eq!(items.len(), 2),
                other => panic!("expected Sequence, got {other:?}"),
            }
        }
        other => panic!("expected Mapping, got {other:?}"),
    }
}
