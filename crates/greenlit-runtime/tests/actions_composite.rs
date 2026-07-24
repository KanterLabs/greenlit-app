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

use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::{Conclusion, EventKind, PlanOptions, SyntheticEvent, plan};
use greenlit_expr::Value;
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
