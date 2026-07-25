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

/// Real-daemon integration test: the command-file protocol
/// (`GITHUB_ENV`/`GITHUB_OUTPUT`/`GITHUB_PATH`/`GITHUB_STEP_SUMMARY`) reaches
/// a Docker action's *sibling* container end to end, including for an action
/// image that runs as a non-root user — the exact case
/// [`crate::executor::cmdfiles::open_to_sibling`] exists for (its own doc
/// comment: "`USER node` is a common last line of an action's Dockerfile").
/// The local action here ends its own Dockerfile with `USER 65534:65534`
/// (`nobody`) and sets its entrypoint via `action.yml`'s `runs.entrypoint`
/// rather than a Dockerfile `ENTRYPOINT`, so the image build itself proves
/// nothing runs as root after that line.
///
/// A preceding `run:` step (in the job container, still root there) has to
/// `mkdir`+`chmod 0777` the directory the action populates: the shared
/// workspace volume is populated by `greenlit-init`'s copy-in as root with
/// ordinary `0755` directories (verified against the real daemon while
/// developing this test), so a `nobody` sibling cannot create a *new* entry
/// directly under `$GITHUB_WORKSPACE` — same permission reality a real
/// non-root Docker action runs into on GitHub's own runners. The action
/// itself still authors the executable file the `GITHUB_PATH` addition
/// proves out.
#[tokio::test]
async fn a_non_root_docker_action_gets_the_full_command_file_protocol() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("a_non_root_docker_action_gets_the_full_command_file_protocol");
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    let action_dir = repo_root.path().join(".github/actions/docker-fidelity");
    std::fs::create_dir_all(&action_dir).unwrap();
    // Root-owned by the build (COPY runs before USER), then made executable
    // while still root -- `nobody` could never chmod a file it doesn't own.
    std::fs::write(
        action_dir.join("Dockerfile"),
        "FROM alpine:3.19\n\
         COPY entrypoint.sh /entrypoint.sh\n\
         RUN chmod 0755 /entrypoint.sh\n\
         USER 65534:65534\n",
    )
    .unwrap();
    std::fs::write(
        action_dir.join("entrypoint.sh"),
        "#!/bin/sh\n\
         set -e\n\
         echo \"verdict=from-docker\" >> \"$GITHUB_OUTPUT\"\n\
         echo \"DOCKER_SET_ENV=yes\" >> \"$GITHUB_ENV\"\n\
         cat > \"$GITHUB_WORKSPACE/docker-path-bin/from-docker-tool\" <<'TOOL'\n\
         #!/bin/sh\n\
         echo tool-from-docker-action\n\
         TOOL\n\
         chmod 0755 \"$GITHUB_WORKSPACE/docker-path-bin/from-docker-tool\"\n\
         echo \"$GITHUB_WORKSPACE/docker-path-bin\" >> \"$GITHUB_PATH\"\n\
         echo \"docker action ran as uid $(id -u)\" >> \"$GITHUB_STEP_SUMMARY\"\n",
    )
    .unwrap();
    // `entrypoint:` here, not a Dockerfile `ENTRYPOINT` -- so the Dockerfile's
    // last instruction really is the `USER` switch to non-root.
    std::fs::write(
        action_dir.join("action.yml"),
        "name: docker fidelity\n\
         description: proves the command-file protocol reaches a non-root Docker action sibling\n\
         runs:\n\
         \x20\x20using: docker\n\
         \x20\x20image: Dockerfile\n\
         \x20\x20entrypoint: /entrypoint.sh\n",
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
         \x20\x20\x20\x20\x20\x20- name: open a directory the non-root action can write into\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20mkdir -p \"$GITHUB_WORKSPACE/docker-path-bin\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20chmod 0777 \"$GITHUB_WORKSPACE/docker-path-bin\"\n\
         \x20\x20\x20\x20\x20\x20- name: docker action sets output, env, path, and a summary\n\
         \x20\x20\x20\x20\x20\x20\x20\x20id: docker\n\
         \x20\x20\x20\x20\x20\x20\x20\x20uses: ./.github/actions/docker-fidelity\n\
         \x20\x20\x20\x20\x20\x20- name: a later run step observes every effect\n\
         \x20\x20\x20\x20\x20\x20\x20\x20run: |\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20set -e\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test \"${{ steps.docker.outputs.verdict }}\" = \"from-docker\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test \"$DOCKER_SET_ENV\" = \"yes\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20case \"$PATH\" in\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\"$GITHUB_WORKSPACE/docker-path-bin\":*) ;;\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20*) echo \"docker-path-bin is not ahead of PATH: $PATH\"; exit 1 ;;\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20esac\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20command -v from-docker-tool\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20test \"$(from-docker-tool)\" = \"tool-from-docker-action\"\n",
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
        volume_namespace: "actions-docker-fidelity".to_string(),
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
        "the non-root docker-action job is green\n--- log ---\n{log}"
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
