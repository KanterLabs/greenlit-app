//! Real-daemon integration test: a local composite action's nested step gets
//! correct input scoping, output mapping, and the *same* job-wide `env:`
//! layers (base/workflow/job, plus a live, GITHUB_PATH-safe `PATH`) an
//! ordinary top-level step sees — not only its own `inputs`/`GITHUB_ENV`
//! accumulation. The container engine is a true external, so it is used
//! real here, not faked (`TESTING.md`).
//!
//! This pins two real defects this test suite discovered (and this crate's
//! own commit fixed) while building `fixtures/actions-ci`
//! (`PHASE-3-actions.md` exit criterion 1): before the fix, a composite
//! step's nested `run:` script (a) never saw the job's base/workflow/job
//! `env:` layers at all — only its own `GITHUB_ENV` accumulation — and (b)
//! resolved its shell against a script path one directory short of where
//! that script was actually written, failing every composite `run:` step
//! outright. Neither had ever been exercised end to end against a real
//! daemon before this file existed.

mod dockerkit;

use std::collections::BTreeSet;
use std::io::Write as _;

use greenlit_actions::manifest::NodeVersion;
use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::{Conclusion, EventKind, PlanOptions, SyntheticEvent, plan};
use greenlit_expr::Value;
use greenlit_runtime::executor::actions::node_runtime::{
    NodeBundleSpecs, NodeVariant, RuntimeBundleError, RuntimeBundleFetcher, RuntimeBundleSpec,
    RuntimeStore,
};
use greenlit_runtime::{IsolationStrategy, ProgressNull, RunConfig, run_plan};

use dockerkit::{engine_if_reachable, notice_no_daemon};

fn synthetic_push_event() -> SyntheticEvent {
    let github = Value::object(vec![
        ("event_name".to_string(), Value::String("push".to_string())),
        (
            "repository".to_string(),
            Value::String("greenlit/actions-composite".to_string()),
        ),
    ]);
    SyntheticEvent {
        kind: EventKind::Push,
        github,
        inputs: Value::object(vec![]),
        deferred_github_properties: BTreeSet::new(),
    }
}

fn runner_env(workspace: &str) -> RunnerEnv {
    RunnerEnv {
        workflow: "actions-composite".to_string(),
        repository: "greenlit/actions-composite".to_string(),
        repository_owner: "greenlit".to_string(),
        sha: "0".repeat(40),
        ref_full: "refs/heads/main".to_string(),
        ref_name: "main".to_string(),
        ref_type: "branch".to_string(),
        event_name: "push".to_string(),
        actor: "tester".to_string(),
        job: String::new(),
        run_id: "1".to_string(),
        run_number: "1".to_string(),
        run_attempt: "1".to_string(),
        workspace: workspace.to_string(),
        runner_name: "greenlit".to_string(),
        runner_temp: "/tmp".to_string(),
        runner_tool_cache: "/opt/hostedtoolcache".to_string(),
        actions_service: None,
    }
}

#[tokio::test]
async fn a_composite_step_sees_job_env_layers_a_live_path_and_maps_its_output() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("a_composite_step_sees_job_env_layers_a_live_path_and_maps_its_output");
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    let action_dir = repo_root.path().join(".github/actions/greet");
    std::fs::create_dir_all(&action_dir).unwrap();
    std::fs::write(
        action_dir.join("action.yml"),
        "name: test composite\n\
         inputs:\n\
         \x20\x20greeting:\n\
         \x20\x20\x20\x20default: hello\n\
         outputs:\n\
         \x20\x20greeting:\n\
         \x20\x20\x20\x20value: ${{ steps.echo.outputs.greeting }}\n\
         runs:\n\
         \x20\x20using: composite\n\
         \x20\x20steps:\n\
         \x20\x20\x20\x20- id: echo\n\
         \x20\x20\x20\x20\x20\x20shell: bash\n\
         \x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20test \"$WORKFLOW_LEVEL\" = \"wf-value\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20test \"$JOB_LEVEL\" = \"job-value\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20grep --version >/dev/null\n\
         \x20\x20\x20\x20\x20\x20\x20\x20echo \"greeting=${{ inputs.greeting }}-nested\" >> \"$GITHUB_OUTPUT\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo_root.path().join(".github/workflows")).unwrap();
    std::fs::write(
        repo_root.path().join(".github/workflows/ci.yml"),
        "on: push\n\
         env:\n\
         \x20\x20WORKFLOW_LEVEL: wf-value\n\
         jobs:\n\
         \x20\x20build:\n\
         \x20\x20\x20\x20runs-on: ubuntu-latest\n\
         \x20\x20\x20\x20env:\n\
         \x20\x20\x20\x20\x20\x20JOB_LEVEL: job-value\n\
         \x20\x20\x20\x20steps:\n\
         \x20\x20\x20\x20\x20\x20- name: add a PATH entry, mirroring setup-node's core.addPath\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20mkdir -p \"$PWD/extra-bin\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20echo \"$PWD/extra-bin\" >> \"$GITHUB_PATH\"\n\
         \x20\x20\x20\x20\x20\x20- name: run composite\n\
         \x20\x20\x20\x20\x20\x20\x20\x20id: composite\n\
         \x20\x20\x20\x20\x20\x20\x20\x20uses: ./.github/actions/greet\n\
         \x20\x20\x20\x20\x20\x20\x20\x20with:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20greeting: hi\n\
         \x20\x20\x20\x20\x20\x20- name: consume the composite's output\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test \"${{ steps.composite.outputs.greeting }}\" = \"hi-nested\"\n",
    )
    .unwrap();

    let workflow = greenlit_workflow::parse_workflow_file_with_name(
        repo_root.path().join(".github/workflows/ci.yml"),
        "ci.yml",
    )
    .expect("parse");
    let event = synthetic_push_event();
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let workspace = "/home/runner/work/actions-composite/actions-composite".to_string();
    let config = RunConfig {
        repo_host_path: repo_root.path().to_path_buf(),
        workspace: workspace.clone(),
        strategy: IsolationStrategy::Auto,
        runner_env: runner_env(&workspace),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "actions-composite".to_string(),
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: dockerkit::test_action_config(),
        store: None,
    };

    let mut log: Vec<u8> = Vec::new();
    let report = run_plan(
        &engine,
        &execution_plan,
        &config,
        &mut log,
        &mut ProgressNull,
    )
    .await
    .expect("run completes");
    let log = String::from_utf8_lossy(&log);

    assert_eq!(
        report.overall,
        Conclusion::Success,
        "the composite job is green\n--- log ---\n{log}"
    );
    let build = &report.jobs[0];
    for step in &build.steps {
        assert_eq!(
            step.conclusion,
            Conclusion::Success,
            "step '{}' must succeed\n--- log ---\n{log}",
            step.label
        );
    }
}

/// `uses:` steps *nested inside a composite action* now support
/// `actions/checkout` and Docker actions
/// (`crate::executor::actions::composite::run_nested_uses`'s
/// `ResolvedUses::Checkout`/`ResolvedUses::Docker` arms) — previously both
/// errored "not supported in v0". This drives one composite action whose own
/// steps are, in order: a nested self-checkout, a nested Dockerfile-built
/// Docker action that writes a marker into the shared workspace and sets a
/// `GITHUB_OUTPUT`, and a nested `run:` step that reads both the marker
/// (workspace visibility from the sibling container) and a file the
/// self-checkout's workspace already carries. The composite's own
/// `outputs:` maps the Docker action's output up, and a later top-level
/// `run:` step reads it back through `steps.<composite-id>.outputs` —
/// proving the round trip survives two layers of nesting. It also asserts
/// the nested checkout's post entry lands on the job-wide LIFO post chain
/// (`crate::executor::actions::post_chain`) exactly like a top-level
/// checkout's does, the same signal `fake_engine_semantics.rs`'s
/// `checkouts_post_step_runs_even_when_a_later_step_fails` asserts on.
#[tokio::test]
async fn a_composites_nested_checkout_and_docker_action_share_the_job_workspace_and_output() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon(
            "a_composites_nested_checkout_and_docker_action_share_the_job_workspace_and_output",
        );
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    std::fs::write(
        repo_root.path().join("README.md"),
        b"nested checkout canary\n",
    )
    .unwrap();

    let docker_action_dir = repo_root.path().join(".github/actions/docker-echo-nested");
    std::fs::create_dir_all(&docker_action_dir).unwrap();
    std::fs::write(
        docker_action_dir.join("action.yml"),
        "name: docker echo nested\n\
         runs:\n\
         \x20\x20using: docker\n\
         \x20\x20image: Dockerfile\n",
    )
    .unwrap();
    std::fs::write(
        docker_action_dir.join("Dockerfile"),
        "FROM alpine:3.19\n\
         ENTRYPOINT [\"sh\", \"-c\", \"echo hello-from-docker > $GITHUB_WORKSPACE/docker-marker.txt && echo marker=hello-from-docker >> $GITHUB_OUTPUT\"]\n",
    )
    .unwrap();

    let composite_dir = repo_root.path().join(".github/actions/nested-uses-demo");
    std::fs::create_dir_all(&composite_dir).unwrap();
    std::fs::write(
        composite_dir.join("action.yml"),
        "name: nested uses demo\n\
         outputs:\n\
         \x20\x20x:\n\
         \x20\x20\x20\x20value: ${{ steps.docker-step.outputs.marker }}\n\
         runs:\n\
         \x20\x20using: composite\n\
         \x20\x20steps:\n\
         \x20\x20\x20\x20- uses: actions/checkout@v4\n\
         \x20\x20\x20\x20- id: docker-step\n\
         \x20\x20\x20\x20\x20\x20uses: ./.github/actions/docker-echo-nested\n\
         \x20\x20\x20\x20- shell: bash\n\
         \x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20test -f \"$GITHUB_WORKSPACE/docker-marker.txt\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20grep -q hello-from-docker \"$GITHUB_WORKSPACE/docker-marker.txt\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20test -f \"$GITHUB_WORKSPACE/README.md\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20grep -q \"nested checkout canary\" \"$GITHUB_WORKSPACE/README.md\"\n",
    )
    .unwrap();

    std::fs::create_dir_all(repo_root.path().join(".github/workflows")).unwrap();
    std::fs::write(
        repo_root.path().join(".github/workflows/ci.yml"),
        "on: push\n\
         jobs:\n\
         \x20\x20build:\n\
         \x20\x20\x20\x20runs-on: ubuntu-latest\n\
         \x20\x20\x20\x20steps:\n\
         \x20\x20\x20\x20\x20\x20- id: composite\n\
         \x20\x20\x20\x20\x20\x20\x20\x20uses: ./.github/actions/nested-uses-demo\n\
         \x20\x20\x20\x20\x20\x20- name: consume the composite's output\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test \"${{ steps.composite.outputs.x }}\" = \"hello-from-docker\"\n",
    )
    .unwrap();

    let workflow = greenlit_workflow::parse_workflow_file_with_name(
        repo_root.path().join(".github/workflows/ci.yml"),
        "ci.yml",
    )
    .expect("parse");
    let event = synthetic_push_event();
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let workspace = "/home/runner/work/actions-composite/actions-composite".to_string();
    let config = RunConfig {
        repo_host_path: repo_root.path().to_path_buf(),
        workspace: workspace.clone(),
        strategy: IsolationStrategy::Auto,
        runner_env: runner_env(&workspace),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "actions-composite-nested".to_string(),
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: dockerkit::test_action_config(),
        store: None,
    };

    let mut log: Vec<u8> = Vec::new();
    let report = run_plan(
        &engine,
        &execution_plan,
        &config,
        &mut log,
        &mut ProgressNull,
    )
    .await
    .expect("run completes");
    let log = String::from_utf8_lossy(&log);

    assert_eq!(
        report.overall,
        Conclusion::Success,
        "the nested-uses job is green\n--- log ---\n{log}"
    );
    let build = &report.jobs[0];
    for step in &build.steps {
        assert_eq!(
            step.conclusion,
            Conclusion::Success,
            "step '{}' must succeed\n--- log ---\n{log}",
            step.label
        );
    }

    // The nested checkout registered its post entry on the same job-wide
    // LIFO chain a top-level checkout uses
    // (`composite::run_nested_uses`'s `ResolvedUses::Checkout` arm), so it
    // must still appear — and run — at job end, same as a top-level
    // checkout's post does.
    let post_step = build
        .steps
        .iter()
        .find(|step| step.label == "Post actions/checkout@v4")
        .unwrap_or_else(|| {
            panic!("the nested checkout's post step must appear in the report\n--- log ---\n{log}")
        });
    assert!(
        post_step.ran,
        "the nested checkout's post step must run\n--- log ---\n{log}"
    );
    assert_eq!(
        build.steps.last().map(|step| step.label.as_str()),
        Some("Post actions/checkout@v4"),
        "post steps run at job end, after every ordinary step\n--- log ---\n{log}"
    );
}

/// Fix 1 (composite input defaults): a composite manifest's declared input
/// `default:` is evaluated as a `${{ }}` template against the enclosing
/// scope that resolved the invoking step's `with:`
/// (`composite::composite_inputs_value`), the same generic default
/// evaluation `nodejs::input_env` already applied to a JS action's inputs
/// (`ActionManifestManager.EvaluateDefaultInput`, `actions/runner` v2.336.0,
/// pinned release). Before this fix, a default was inserted as a raw
/// literal, so `default: ${{ github.repository }}` — left unset in `with:`
/// here — would have resolved to the four literal characters `${{` instead
/// of the run's actual repository.
#[tokio::test]
async fn a_composite_inputs_default_is_evaluated_as_a_template_not_a_literal() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("a_composite_inputs_default_is_evaluated_as_a_template_not_a_literal");
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    let action_dir = repo_root.path().join(".github/actions/default-echo");
    std::fs::create_dir_all(&action_dir).unwrap();
    std::fs::write(
        action_dir.join("action.yml"),
        "name: default echo\n\
         inputs:\n\
         \x20\x20greeting:\n\
         \x20\x20\x20\x20default: ${{ github.repository }}\n\
         outputs:\n\
         \x20\x20greeting:\n\
         \x20\x20\x20\x20value: ${{ steps.echo.outputs.greeting }}\n\
         runs:\n\
         \x20\x20using: composite\n\
         \x20\x20steps:\n\
         \x20\x20\x20\x20- id: echo\n\
         \x20\x20\x20\x20\x20\x20shell: bash\n\
         \x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20echo \"greeting=${{ inputs.greeting }}\" >> \"$GITHUB_OUTPUT\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo_root.path().join(".github/workflows")).unwrap();
    std::fs::write(
        repo_root.path().join(".github/workflows/ci.yml"),
        "on: push\n\
         jobs:\n\
         \x20\x20build:\n\
         \x20\x20\x20\x20runs-on: ubuntu-latest\n\
         \x20\x20\x20\x20steps:\n\
         \x20\x20\x20\x20\x20\x20- id: composite\n\
         \x20\x20\x20\x20\x20\x20\x20\x20uses: ./.github/actions/default-echo\n\
         \x20\x20\x20\x20\x20\x20- name: assert the default resolved to the real repository\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test \"${{ steps.composite.outputs.greeting }}\" = \"greenlit/actions-composite\"\n",
    )
    .unwrap();

    let workflow = greenlit_workflow::parse_workflow_file_with_name(
        repo_root.path().join(".github/workflows/ci.yml"),
        "ci.yml",
    )
    .expect("parse");
    let event = synthetic_push_event();
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let workspace = "/home/runner/work/actions-composite/actions-composite".to_string();
    let config = RunConfig {
        repo_host_path: repo_root.path().to_path_buf(),
        workspace: workspace.clone(),
        strategy: IsolationStrategy::Auto,
        runner_env: runner_env(&workspace),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "actions-composite-default".to_string(),
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: dockerkit::test_action_config(),
        store: None,
    };

    let mut log: Vec<u8> = Vec::new();
    let report = run_plan(
        &engine,
        &execution_plan,
        &config,
        &mut log,
        &mut ProgressNull,
    )
    .await
    .expect("run completes");
    let log = String::from_utf8_lossy(&log);

    assert_eq!(
        report.overall,
        Conclusion::Success,
        "the default must resolve to a real value the consuming step accepts\n--- log ---\n{log}"
    );
}

/// Fix 2 (nested step status semantics): verified against the pinned
/// runner's `CompositeActionHandler.RunStepsAsync`/`SuccessFunction`/
/// `FailureFunction` (`actions/runner` v2.336.0) — see
/// `composite::build_composite_context`'s doc comment for the full source
/// citation this test pins. After one nested step fails (no
/// `continue-on-error`): a later nested step whose `if:` explicitly
/// references a status function (`always()`) must still activate, evaluated
/// against *this composite's own* rolling status, not the enclosing job's
/// status at composite entry (which is still `success` here — the composite
/// step itself hasn't failed yet from the job's point of view); a later
/// nested step with no `if:` at all (implicitly `success()`) must not. The
/// composite step's own overall outcome still follows "first failure wins".
#[tokio::test]
async fn a_failed_nested_step_gates_later_steps_by_the_composites_own_rolling_status() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon(
            "a_failed_nested_step_gates_later_steps_by_the_composites_own_rolling_status",
        );
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    let action_dir = repo_root.path().join(".github/actions/status-demo");
    std::fs::create_dir_all(&action_dir).unwrap();
    std::fs::write(
        action_dir.join("action.yml"),
        "name: status demo\n\
         runs:\n\
         \x20\x20using: composite\n\
         \x20\x20steps:\n\
         \x20\x20\x20\x20- id: step1\n\
         \x20\x20\x20\x20\x20\x20shell: bash\n\
         \x20\x20\x20\x20\x20\x20run: exit 1\n\
         \x20\x20\x20\x20- id: step2\n\
         \x20\x20\x20\x20\x20\x20if: always()\n\
         \x20\x20\x20\x20\x20\x20shell: bash\n\
         \x20\x20\x20\x20\x20\x20run: touch \"$GITHUB_WORKSPACE/step2-marker\"\n\
         \x20\x20\x20\x20- id: step3\n\
         \x20\x20\x20\x20\x20\x20shell: bash\n\
         \x20\x20\x20\x20\x20\x20run: touch \"$GITHUB_WORKSPACE/step3-marker\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo_root.path().join(".github/workflows")).unwrap();
    std::fs::write(
        repo_root.path().join(".github/workflows/ci.yml"),
        "on: push\n\
         jobs:\n\
         \x20\x20build:\n\
         \x20\x20\x20\x20runs-on: ubuntu-latest\n\
         \x20\x20\x20\x20steps:\n\
         \x20\x20\x20\x20\x20\x20- id: composite\n\
         \x20\x20\x20\x20\x20\x20\x20\x20uses: ./.github/actions/status-demo\n\
         \x20\x20\x20\x20\x20\x20- id: verify\n\
         \x20\x20\x20\x20\x20\x20\x20\x20if: always()\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test -f \"$GITHUB_WORKSPACE/step2-marker\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test ! -f \"$GITHUB_WORKSPACE/step3-marker\"\n",
    )
    .unwrap();

    let workflow = greenlit_workflow::parse_workflow_file_with_name(
        repo_root.path().join(".github/workflows/ci.yml"),
        "ci.yml",
    )
    .expect("parse");
    let event = synthetic_push_event();
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let workspace = "/home/runner/work/actions-composite/actions-composite".to_string();
    let config = RunConfig {
        repo_host_path: repo_root.path().to_path_buf(),
        workspace: workspace.clone(),
        strategy: IsolationStrategy::Auto,
        runner_env: runner_env(&workspace),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "actions-composite-status".to_string(),
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: dockerkit::test_action_config(),
        store: None,
    };

    let mut log: Vec<u8> = Vec::new();
    let report = run_plan(
        &engine,
        &execution_plan,
        &config,
        &mut log,
        &mut ProgressNull,
    )
    .await
    .expect("run completes");
    let log = String::from_utf8_lossy(&log);

    assert_eq!(
        report.overall,
        Conclusion::Failure,
        "the composite's own nested failure must fail the job\n--- log ---\n{log}"
    );
    let build = &report.jobs[0];
    let composite_step = build
        .steps
        .iter()
        .find(|step| step.label == "composite")
        .unwrap_or_else(|| {
            panic!("the composite step must appear in the report\n--- log ---\n{log}")
        });
    assert_eq!(
        composite_step.conclusion,
        Conclusion::Failure,
        "the composite step's own outcome follows its first nested failure ('first failure wins')\n--- log ---\n{log}"
    );
    let verify_step = build
        .steps
        .iter()
        .find(|step| step.label == "verify")
        .unwrap_or_else(|| panic!("the verify step must appear in the report\n--- log ---\n{log}"));
    assert_eq!(
        verify_step.conclusion,
        Conclusion::Success,
        "step2's marker must exist (its `if: always()` activated after step1 failed) and \
         step3's must not (its implicit `success()` gate stayed closed)\n--- log ---\n{log}"
    );

    // Fix 2's other half: nested steps previously emitted no per-step log
    // line of their own at all.
    assert!(
        log.contains("\u{25b6} step1"),
        "step1's own header line must appear in the captured log\n--- log ---\n{log}"
    );
    assert!(
        log.contains("\u{25b6} step2"),
        "step2's own header line must appear in the captured log\n--- log ---\n{log}"
    );
    assert!(
        log.contains("\u{2013} step3 (skipped)"),
        "step3 must be recorded as skipped in the captured log, not silently absent\n--- log ---\n{log}"
    );
}

/// Fix 3 (nested JS env layers): a nested JavaScript action's pre/main
/// phases must see the *same* fully-layered environment a top-level JS
/// action or a nested `run:` step already gets — the job's base/`GITHUB_*`
/// layer (`GITHUB_REPOSITORY` here) and the job's own `env:` (`JOB_LEVEL`
/// here), not only the `GITHUB_ENV` accumulation
/// (`composite::run_nested_uses`'s `ResolvedUses::Node` arm, `full_env` was
/// `state.job_accumulated` alone before this fix). The fake `node` binary
/// below ignores its script-path argument and instead echoes both variables
/// straight into `GITHUB_OUTPUT`, so a passing `test` step downstream proves
/// they were visible in the actual container process environment, not just
/// in the expression `env` context.
#[tokio::test]
async fn a_nested_js_actions_pre_and_main_phases_see_the_jobs_full_env_layers() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("a_nested_js_actions_pre_and_main_phases_see_the_jobs_full_env_layers");
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    let js_action_dir = repo_root.path().join(".github/actions/env-probe-js");
    std::fs::create_dir_all(&js_action_dir).unwrap();
    std::fs::write(
        js_action_dir.join("action.yml"),
        "name: env probe\nruns:\n  using: node20\n  main: main.js\n",
    )
    .unwrap();
    std::fs::write(
        js_action_dir.join("main.js"),
        b"// never executed by the fake node runtime\n",
    )
    .unwrap();

    let composite_dir = repo_root.path().join(".github/actions/env-layers-demo");
    std::fs::create_dir_all(&composite_dir).unwrap();
    std::fs::write(
        composite_dir.join("action.yml"),
        "name: env layers demo\n\
         outputs:\n\
         \x20\x20repo:\n\
         \x20\x20\x20\x20value: ${{ steps.probe.outputs.repo }}\n\
         \x20\x20job_level:\n\
         \x20\x20\x20\x20value: ${{ steps.probe.outputs.job_level }}\n\
         runs:\n\
         \x20\x20using: composite\n\
         \x20\x20steps:\n\
         \x20\x20\x20\x20- id: probe\n\
         \x20\x20\x20\x20\x20\x20uses: ./.github/actions/env-probe-js\n",
    )
    .unwrap();

    std::fs::create_dir_all(repo_root.path().join(".github/workflows")).unwrap();
    std::fs::write(
        repo_root.path().join(".github/workflows/ci.yml"),
        "on: push\n\
         jobs:\n\
         \x20\x20build:\n\
         \x20\x20\x20\x20runs-on: ubuntu-latest\n\
         \x20\x20\x20\x20env:\n\
         \x20\x20\x20\x20\x20\x20JOB_LEVEL: job-value\n\
         \x20\x20\x20\x20steps:\n\
         \x20\x20\x20\x20\x20\x20- id: composite\n\
         \x20\x20\x20\x20\x20\x20\x20\x20uses: ./.github/actions/env-layers-demo\n\
         \x20\x20\x20\x20\x20\x20- name: assert the nested JS action saw both env layers\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test \"${{ steps.composite.outputs.repo }}\" = \"greenlit/actions-composite\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test \"${{ steps.composite.outputs.job_level }}\" = \"job-value\"\n",
    )
    .unwrap();

    let workflow = greenlit_workflow::parse_workflow_file_with_name(
        repo_root.path().join(".github/workflows/ci.yml"),
        "ci.yml",
    )
    .expect("parse");
    let event = synthetic_push_event();
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let workspace = "/home/runner/work/actions-composite/actions-composite".to_string();
    let store_root = tempfile::tempdir().unwrap();
    let config = RunConfig {
        repo_host_path: repo_root.path().to_path_buf(),
        workspace: workspace.clone(),
        strategy: IsolationStrategy::Auto,
        runner_env: runner_env(&workspace),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "actions-composite-env-probe".to_string(),
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: env_probe_action_config(store_root.path()),
        store: None,
    };

    let mut log: Vec<u8> = Vec::new();
    let report = run_plan(
        &engine,
        &execution_plan,
        &config,
        &mut log,
        &mut ProgressNull,
    )
    .await
    .expect("run completes");
    let log = String::from_utf8_lossy(&log);

    assert_eq!(
        report.overall,
        Conclusion::Success,
        "the nested JS action must have seen GITHUB_REPOSITORY and JOB_LEVEL\n--- log ---\n{log}"
    );
}

/// A fake `node` executable (mirrors `tests/actions_nodejs.rs`'s own
/// `FAKE_NODE_SCRIPT`): ignores its script-path argument entirely and
/// instead proves the *nested* JS action's env layering directly, echoing
/// `GITHUB_REPOSITORY` (the job's base/runner layer) and `JOB_LEVEL` (the
/// job's own `env:`) into `GITHUB_OUTPUT`.
const ENV_PROBE_NODE_SCRIPT: &str = "#!/bin/sh\n\
echo \"repo=$GITHUB_REPOSITORY\" >> \"$GITHUB_OUTPUT\"\n\
echo \"job_level=$JOB_LEVEL\" >> \"$GITHUB_OUTPUT\"\n";

/// Builds a tiny gzip-compressed tar containing `bin/node` (the fake script
/// above), mirroring the real bundles' `bin/node` layout
/// (`tests/actions_nodejs.rs`'s own `fake_node_bundle_bytes`).
fn fake_node_bundle_bytes() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_path("bin/node").unwrap();
    header.set_size(ENV_PROBE_NODE_SCRIPT.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append(&header, ENV_PROBE_NODE_SCRIPT.as_bytes())
        .unwrap();
    let tar_bytes = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar_bytes).unwrap();
    encoder.finish().unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Serves the fake bundle above for every `(version, variant)` asked for.
struct FakeBundleFetcher {
    bytes: Vec<u8>,
}

#[async_trait::async_trait]
impl RuntimeBundleFetcher for FakeBundleFetcher {
    async fn download(&self, _spec: &RuntimeBundleSpec) -> Result<Vec<u8>, RuntimeBundleError> {
        Ok(self.bytes.clone())
    }
}

/// Names the *fake* bundle's own checksum instead of the real pinned one,
/// so the fake bundle passes the real checksum-verification step.
struct FakeBundleSpecs {
    sha256: String,
}

impl NodeBundleSpecs for FakeBundleSpecs {
    fn spec(&self, version: NodeVersion, variant: NodeVariant) -> RuntimeBundleSpec {
        RuntimeBundleSpec {
            version,
            variant,
            url: "https://example.invalid/fake-node.tar.gz",
            sha256: Box::leak(self.sha256.clone().into_boxed_str()),
            strip_top_level: false,
        }
    }
}

/// An [`greenlit_runtime::ActionRuntimeConfig`] that resolves only local
/// (`./...`) actions — same shape as `dockerkit::test_action_config`,
/// except the node-runtime fetcher/spec pair is wired to the fake bundle
/// above instead of erroring, so a nested JS action actually executes.
fn env_probe_action_config(store_root: &std::path::Path) -> greenlit_runtime::ActionRuntimeConfig {
    use greenlit_actions::CommitSha;
    use greenlit_actions::resolve::{RefResolver, ResolveError};
    use greenlit_actions::store::{ActionFetcher, ActionStore, FetchError};

    struct NeverResolves;
    #[async_trait::async_trait]
    impl RefResolver for NeverResolves {
        async fn resolve(
            &self,
            owner: &str,
            repo: &str,
            git_ref: &str,
        ) -> Result<CommitSha, ResolveError> {
            Err(ResolveError::NotFound {
                owner: owner.to_string(),
                repo: repo.to_string(),
                git_ref: git_ref.to_string(),
            })
        }
    }
    struct NeverFetches;
    #[async_trait::async_trait]
    impl ActionFetcher for NeverFetches {
        async fn fetch(
            &self,
            owner: &str,
            repo: &str,
            sha: &CommitSha,
            _dest: &std::path::Path,
        ) -> Result<(), FetchError> {
            Err(FetchError::Download {
                owner: owner.to_string(),
                repo: repo.to_string(),
                sha: sha.as_str().to_string(),
                message: "not used: this test only references local actions".to_string(),
            })
        }
    }

    let bytes = fake_node_bundle_bytes();
    let sha256 = sha256_hex(&bytes);
    greenlit_runtime::ActionRuntimeConfig {
        resolver: std::sync::Arc::new(NeverResolves),
        store: ActionStore::at(store_root.join("actions")),
        fetcher: std::sync::Arc::new(NeverFetches),
        node_runtime_fetcher: std::sync::Arc::new(FakeBundleFetcher { bytes }),
        node_runtime_specs: std::sync::Arc::new(FakeBundleSpecs { sha256 }),
        node_runtime_store: RuntimeStore::at(store_root.join("node-runtimes")),
        github_token: None,
    }
}
