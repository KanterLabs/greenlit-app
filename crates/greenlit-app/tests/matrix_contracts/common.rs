//! Shared rich workflows and thin public-CLI helpers.

use super::support;
use super::support::Sandbox;

pub(super) const EXPRESSION_MATRICES: &str = r#"on: push
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
  tagged_values:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        value:
          - null
          - false
          - 1.5
          - text
          - [nested]
          - {nested: true}
          - .nan
          - .inf
          - -.inf
    steps:
      - run: echo tagged
"#;

pub(super) const INCLUDE_ONLY_STRATEGY: &str = r#"on: push
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

pub(super) const DEFAULT_MAX_PARALLEL: &str = r#"on: push
jobs:
  default_parallel:
    name: ${{ strategy.job-total }}-${{ strategy.max-parallel }}
    runs-on: ubuntu-latest
    strategy:
      matrix:
        channel: [stable, beta, nightly]
    steps:
      - run: echo ${{ strategy.max-parallel }}
"#;

pub(super) const DEFERRED_MATRICES: &str = r#"on: push
jobs:
  producer:
    runs-on: ubuntu-latest
    if: false
    outputs:
      matrix: ${{ steps.values.outputs.matrix }}
      colors: ${{ steps.values.outputs.colors }}
      fail_fast: ${{ steps.values.outputs.fail_fast }}
      max_parallel: ${{ steps.values.outputs.max_parallel }}
    steps:
      - id: values
        run: echo produce
  whole:
    needs: producer
    name: whole-${{ matrix.color }}-${{ strategy.job-total }}
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: ${{ fromJSON(needs.producer.outputs.fail_fast) }}
      max-parallel: ${{ fromJSON(needs.producer.outputs.max_parallel) }}
      matrix: ${{ fromJSON(needs.producer.outputs.matrix) }}
    steps:
      - run: echo ${{ matrix.color }} ${{ strategy.job-index }}
  inline:
    needs: producer
    runs-on: ubuntu-latest
    strategy:
      max-parallel: 2
      matrix:
        color: ${{ fromJSON(needs.producer.outputs.colors) }}
    steps:
      - run: echo ${{ matrix.color }} ${{ strategy.job-total }}
      - run: echo controls ${{ strategy.fail-fast }} ${{ strategy.max-parallel }}
  static_matrix_deferred_control:
    needs: producer
    name: ${{ strategy.job-total }}-${{ strategy.max-parallel }}
    runs-on: ubuntu-latest
    strategy:
      fail-fast: ${{ fromJSON(needs.producer.outputs.fail_fast) }}
      matrix:
        channel: [stable, beta]
    steps:
      - run: echo ${{ strategy.job-index }}
  linted:
    needs: producer
    name: ${{ needs.producer.outputs.missing_fail_fast }}
    runs-on: ubuntu-latest
    env:
      MISSING: ${{ needs.producer.outputs.missing_max_parallel }}
    steps:
      - run: echo ${{ needs[format('{0}', 'producer')].outputs[format('{0}', 'missing_axis')] }}
  after_inline:
    needs: inline
    runs-on: ubuntu-latest
    steps:
      - run: echo unreachable
"#;

pub(super) const CONTEXT_SHAPES: &str = r#"on: push
jobs:
  producer:
    runs-on: ubuntu-latest
    outputs:
      declared: ${{ steps.emit.outputs.declared }}
    steps:
      - id: emit
        run: echo produce
  observer:
    runs-on: ubuntu-latest
    env:
      NO_NEEDS: ${{ needs.producer.outputs.declared }}
    steps:
      - run: echo observe
  consumer:
    needs: producer
    runs-on: ubuntu-latest
    env:
      DIRECT: ${{ needs[format('{0}', 'producer')].outputs[format('{0}', 'declared')] }}
      RESULT: ${{ needs.producer.result }}
      RESULT_INDEXED: ${{ needs.producer.result[fromJSON('bad')] }}
      RESULT_RUNTIME_INDEX: ${{ needs.producer.result[github.workspace] }}
      UNDECLARED: ${{ needs.producer.outputs.missing }}
      NON_DIRECT: ${{ needs.observer.outputs.whatever }}
    outputs:
      first_value: ${{ steps.first.outputs.value }}
      first_whole: ${{ toJSON(steps.first) }}
      second_result: ${{ steps.second.conclusion }}
      missing_value: ${{ steps.missing[fromJSON('bad')] }}
    steps:
      - id: first
        run: echo current=${{ steps.first.outputs.value }} future=${{ steps.second.outputs.value }} missing=${{ steps.missing.outputs.value }} skipped-key=${{ steps.missing[runner.os] }}
      - id: second
        run: echo prior=${{ steps[format('{0}', 'first')].outputs[format('{0}', 'value')] }} current=${{ steps.second.outputs.value }} future=${{ steps.future.outputs.value }} missing=${{ steps.missing.outputs.value }}
  strategy_shape:
    needs: producer
    runs-on: ubuntu-latest
    strategy:
      fail-fast: ${{ needs.producer.outputs.declared == 'yes' }}
      matrix:
        axis: [one, two]
    steps:
      - run: echo total=${{ strategy[format('{0}', 'job-total')] }}
      - run: echo fail=${{ strategy[format('{0}', 'fail-fast')] }}
"#;

pub(super) fn sandbox_with_workflow(source: &str) -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write("matrix.yml", source);
    sandbox.init_git();
    sandbox
}

pub(super) fn plan_json(source: &str) -> serde_json::Value {
    let sandbox = sandbox_with_workflow(source);
    let output = sandbox.run(&["plan", "-W", "matrix.yml", "--json"]);
    assert!(
        output.status.success(),
        "plan failed: {}",
        support::stderr_text(&output)
    );
    serde_json::from_slice(&output.stdout).expect("plan stdout must be valid JSON")
}

pub(super) fn job<'a>(plan: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    plan["jobs"]
        .as_array()
        .expect("jobs array")
        .iter()
        .find(|job| job["id"] == id)
        .unwrap_or_else(|| panic!("job '{id}' missing from plan"))
}

pub(super) fn run_script(leg: &serde_json::Value) -> &str {
    leg["steps"][0]["kind"]["script"]["value"]
        .as_str()
        .expect("resolved run script")
}

pub(super) fn whole_matrix_expression_workflow(matrix_json: &str) -> String {
    format!(
        "on: push\njobs:\n  matrix:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix: ${{{{ fromJSON('{matrix_json}') }}}}\n    steps:\n      - run: echo ${{{{ matrix.index }}}}\n"
    )
}
