//! Real-daemon integration test: a Docker action runs as a sibling
//! container and shares the job's live workspace with it
//! (`crate::executor::actions::docker_action` module docs' shared-volume
//! design) — a `run:` step's write is visible to the Docker action, and the
//! Docker action's write is visible back to a later `run:` step in the job
//! container. The container engine is a true external, so it is used real
//! here, not faked (`TESTING.md`).

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
            Value::String("greenlit/actions-docker".to_string()),
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
        workflow: "actions-docker".to_string(),
        repository: "greenlit/actions-docker".to_string(),
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
async fn a_docker_action_shares_the_live_workspace_with_the_job_container() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("a_docker_action_shares_the_live_workspace_with_the_job_container");
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    let action_dir = repo_root.path().join(".github/actions/docker-echo");
    std::fs::create_dir_all(&action_dir).unwrap();
    std::fs::write(
        action_dir.join("action.yml"),
        "name: docker echo\n\
         runs:\n\
         \x20\x20using: docker\n\
         \x20\x20image: docker://alpine:3.19\n\
         \x20\x20args:\n\
         \x20\x20\x20\x20- sh\n\
         \x20\x20\x20\x20- -c\n\
         \x20\x20\x20\x20- cat $GITHUB_WORKSPACE/shared.txt && echo world >> $GITHUB_WORKSPACE/shared.txt\n",
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
         \x20\x20\x20\x20\x20\x20- name: write from the job container\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: echo hello > \"$GITHUB_WORKSPACE/shared.txt\"\n\
         \x20\x20\x20\x20\x20\x20- name: docker action reads and appends\n\
         \x20\x20\x20\x20\x20\x20\x20\x20uses: ./.github/actions/docker-echo\n\
         \x20\x20\x20\x20\x20\x20- name: read back from the job container\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20cat \"$GITHUB_WORKSPACE/shared.txt\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20grep -q hello \"$GITHUB_WORKSPACE/shared.txt\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20grep -q world \"$GITHUB_WORKSPACE/shared.txt\"\n",
    )
    .unwrap();

    let workflow = greenlit_workflow::parse_workflow_file_with_name(
        repo_root.path().join(".github/workflows/ci.yml"),
        "ci.yml",
    )
    .expect("parse");
    let event = synthetic_push_event();
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let workspace = "/home/runner/work/actions-docker/actions-docker".to_string();
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
        volume_namespace: "actions-docker".to_string(),
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
        "the docker-action job is green\n--- log ---\n{log}"
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
