//! Shared rich workflow and thin public-CLI helpers.

use super::support;
use super::support::Sandbox;

pub(super) const RICH_WORKFLOW: &str = r#"on:
  pull_request:
  workflow_dispatch:
    inputs:
      required_text:
        required: true
        type: string
        default: hello
      enabled:
        required: true
        type: boolean
        default: true
      count:
        type: number
        default: 2.5
      mode:
        type: choice
        options: [fast, slow]
        default: fast
env:
  LEVEL: workflow
jobs:
  workflow_env:
    runs-on: ubuntu-latest
    steps:
      - id: workflow_layer
        run: echo ${{ env.LEVEL }}
  skipped:
    runs-on: ubuntu-latest
    if: false
    steps:
      - run: echo skipped
  dependent:
    needs: [skipped, skipped]
    runs-on: ubuntu-latest
    env:
      LEVEL: job
    steps:
      - id: job_layer
        run: echo ${{ env.LEVEL }}
      - id: step_layer
        env:
          LEVEL: step
        run: echo ${{ env.LEVEL }}
      - id: no_if
        run: echo implicit
      - id: status_if
        if: always()
        run: echo explicit
  rescued:
    needs: skipped
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo rescued
  zero:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        axis: [only]
        exclude:
          - axis: only
          - axis: missing
    steps:
      - run: echo never
  supported_label_list:
    runs-on: [ubuntu-latest]
    steps:
      - run: echo supported
  runner_source:
    runs-on: ubuntu-latest
    outputs:
      label: ${{ steps.select.outputs.label }}
    steps:
      - id: select
        run: echo "label=ubuntu-22.04" >> "$GITHUB_OUTPUT"
  deferred_runner:
    needs: runner_source
    runs-on: ${{ needs.runner_source.outputs.label }}
    steps:
      - run: echo deferred runner
  pr_shape:
    runs-on: ubuntu-latest
    if: github.event_name != 'pull_request' || (github.event.pull_request.number == 1 && github.base_ref == 'main' && github.head_ref == 'main' && github.workflow_ref == format('{0}/contracts.yml@refs/pull/1/merge', github.repository))
    steps:
      - run: echo pull-request
  dispatch_shape:
    runs-on: ubuntu-latest
    if: github.event_name != 'workflow_dispatch' || (inputs.required_text == 'hello' && inputs.enabled == true && inputs.count == 2.5 && inputs.mode == 'fast' && github.event.inputs.enabled == 'true' && github.event.inputs.count == '2.5' && github.server_url == 'https://github.com' && github.api_url == 'https://api.github.com' && github.graphql_url == 'https://api.github.com/graphql' && github.triggering_actor == 'litci tests' && github.workflow_sha == github.sha && github.workflow == 'contracts.yml' && github.workflow_ref == format('{0}/contracts.yml@refs/heads/main', github.repository))
    steps:
      - run: echo dispatch
  github_runtime:
    runs-on: ubuntu-latest
    steps:
      - id: runtime_context
        env:
          WHOLE_GITHUB: ${{ toJSON(github) }}
          RUNTIME_FIELDS: >-
            ${{ github.action }} ${{ github.action_path }} ${{ github.action_ref }} ${{ github.action_repository }} ${{ github.action_status }}
            ${{ github.actor_id }} ${{ github.env }} ${{ github.event_path }} ${{ github.job }} ${{ github.path }}
            ${{ github.ref_protected }} ${{ github.repository_id }} ${{ github.repository_owner_id }} ${{ github.repositoryUrl }}
            ${{ github.retention_days }} ${{ github.run_attempt }} ${{ github.run_id }} ${{ github.run_number }}
            ${{ github.secret_source }} ${{ github.token }} ${{ github.workspace }}
        run: echo runtime context
  render_fields:
    name: Render ${{ github.event_name }}
    runs-on: ubuntu-latest
    permissions:
      contents: write
    container:
      image: alpine:3.20
      credentials:
        username: fixture-user
        password: fixture-password
      env:
        CONTAINER_EVENT: ${{ github.event_name }}
      ports: ["8080:80"]
      volumes: ["/tmp:/tmp"]
      options: --cpus 1
    services:
      redis:
        image: redis:7
        env:
          SERVICE_EVENT: ${{ github.event_name }}
        ports: ["6379:6379"]
        volumes: ["/var/lib/redis:/data"]
        options: --health-cmd redis-cli ping
    env:
      JOB_EVENT: ${{ github.event_name }}
    defaults:
      run:
        shell: sh
        working-directory: job-work
    outputs:
      static-output: rendered
      deferred-output: ${{ steps.rich.outputs.value }}
    steps:
      - id: rich
        name: Rich ${{ github.event_name }}
        if: github.event_name == 'workflow_dispatch'
        env:
          STEP_EVENT: ${{ github.event_name }}
        working-directory: step-work
        continue-on-error: ${{ fromJSON('false') }}
        timeout-minutes: ${{ fromJSON('30') }}
        shell: bash
        run: |
          echo first
          echo second
      - id: action
        name: Use action
        uses: actions/checkout@0123456789abcdef0123456789abcdef01234567
        with:
          ref: ${{ github.ref }}
          fetch-depth: 1
  render_matrix:
    name: Matrix ${{ matrix.channel }}
    runs-on: ubuntu-latest
    strategy:
      matrix:
        channel: [stable]
    container:
      image: alpine:${{ matrix.channel }}
    services:
      redis:
        image: redis:${{ matrix.channel }}
    env:
      MATRIX_CHANNEL: ${{ matrix.channel }}
    defaults:
      run:
        shell: sh
        working-directory: matrix-work
    outputs:
      selected: ${{ matrix.channel }}
    steps:
      - name: Matrix step ${{ matrix.channel }}
        run: echo ${{ matrix.channel }}
  env_runtime:
    needs: runner_source
    runs-on: ubuntu-latest
    env:
      FOO: job
      DEFERRED: ${{ needs.runner_source.outputs.label }}
    steps:
      - id: initial_env
        env:
          STATIC_COMPUTED: ${{ env[format('{0}', 'FOO')] }}
          DYNAMIC_KEY: ${{ env[needs.runner_source.outputs.label] }}
          WRONG_CASE: ${{ env.foo }}
          FIRST_UNSET: ${{ env.UNSET }}
        run: echo initial=${{ env[format('{0}', 'FOO')] }} unset=${{ env.UNSET }}
      - id: deferred_after_runnable
        run: echo ${{ env.DEFERRED }}
      - id: mutate_env
        run: echo "FOO=new" >> "$GITHUB_ENV"
      - id: after_mutation
        run: echo ${{ env[format('{0}', 'FOO')] }}
      - id: step_override
        env:
          FOO: fixed
        run: echo ${{ env[format('{0}', 'FOO')] }}
run-name: Run ${{ inputs.mode }} by ${{ github.actor }}
defaults:
  run:
    shell: bash
    working-directory: workflow-work
permissions:
  contents: read
  actions: write
"#;

pub(super) fn sandbox_with_workflow(source: &str) -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write("contracts.yml", source);
    sandbox.init_git();
    sandbox
}

pub(super) fn plan_json(
    sandbox: &Sandbox,
    extra_args: &[&str],
) -> (serde_json::Value, String, String) {
    let mut args = vec!["plan", "-W", "contracts.yml", "--json"];
    args.extend_from_slice(extra_args);
    let output = sandbox.run(&args);
    assert!(
        output.status.success(),
        "plan failed: {}",
        support::stderr_text(&output)
    );
    let plan = serde_json::from_slice(&output.stdout).expect("plan stdout must be one JSON value");
    (
        plan,
        support::stdout_text(&output),
        support::stderr_text(&output),
    )
}

pub(super) fn job<'a>(plan: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    plan["jobs"]
        .as_array()
        .expect("jobs array")
        .iter()
        .find(|job| job["id"] == id)
        .unwrap_or_else(|| panic!("job '{id}' missing from plan"))
}

pub(super) fn step<'a>(job: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    job["steps"]
        .as_array()
        .expect("steps array")
        .iter()
        .find(|step| step["id"] == id)
        .unwrap_or_else(|| panic!("step '{id}' missing from plan"))
}
