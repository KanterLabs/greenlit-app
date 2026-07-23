//! Real-daemon smoke test: the `fixtures/shell-ci` workflow runs green end to
//! end through the executor (`PHASE-2-execution.md` exit criterion 1).
//!
//! This exercises the whole Phase 2 path against a live engine: base and custom
//! job containers, overlay/copy-in isolation, shell resolution, env layering,
//! `GITHUB_OUTPUT`/`GITHUB_ENV` command files, a `::group::` block,
//! `continue-on-error`, `if: failure()`, and a declared job output propagated to
//! a direct `needs` dependent. The engine is a true external, so it is used real
//! here, not faked (`TESTING.md`).

mod dockerkit;

use std::collections::BTreeSet;
use std::path::PathBuf;

use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::{
    Conclusion, EventKind, PlanOptions, SyntheticEvent, plan, validate_v0_support,
};
use greenlit_expr::Value;
use greenlit_runtime::{IsolationStrategy, RunConfig, run_plan};

use dockerkit::{engine_if_reachable, notice_no_daemon};

fn fixture_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    std::fs::canonicalize(format!("{manifest}/../../fixtures/shell-ci"))
        .expect("shell-ci fixture exists")
}

fn synthetic_push_event() -> SyntheticEvent {
    let github = Value::object(vec![
        ("event_name".to_string(), Value::String("push".to_string())),
        (
            "repository".to_string(),
            Value::String("greenlit/shell-ci".to_string()),
        ),
        (
            "repository_owner".to_string(),
            Value::String("greenlit".to_string()),
        ),
        ("sha".to_string(), Value::String("0".repeat(40))),
        (
            "ref".to_string(),
            Value::String("refs/heads/main".to_string()),
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
        workflow: "shell-ci".to_string(),
        repository: "greenlit/shell-ci".to_string(),
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
    }
}

#[tokio::test]
async fn shell_ci_fixture_runs_green_end_to_end() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("shell_ci_fixture_runs_green_end_to_end");
        return;
    };

    let root = fixture_root();
    let workflow_path = root.join(".github/workflows/ci.yml");
    let workflow =
        greenlit_workflow::parse_workflow_file_with_name(&workflow_path, "ci.yml").expect("parse");
    validate_v0_support(&workflow).expect("v0 supported");

    let event = synthetic_push_event();
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let workspace = "/home/runner/work/shell-ci/shell-ci".to_string();
    let config = RunConfig {
        repo_host_path: root.clone(),
        workspace: workspace.clone(),
        // Copy-in is available everywhere; the daemon here lacks unprivileged
        // overlayfs, so Auto falls back to it deterministically.
        strategy: IsolationStrategy::Auto,
        runner_env: runner_env(&workspace),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "shell-ci-smoke".to_string(),
        write_back: false,
    };

    let mut log: Vec<u8> = Vec::new();
    let report = run_plan(&engine, &execution_plan, &config, &mut log)
        .await
        .expect("run completes");
    let log = String::from_utf8_lossy(&log);

    assert_eq!(
        report.overall,
        Conclusion::Success,
        "the whole run is green\n--- log ---\n{log}"
    );

    let build = report
        .jobs
        .iter()
        .find(|job| job.id == "build")
        .expect("build job ran");
    assert_eq!(build.result, Conclusion::Success, "build is green");
    assert_eq!(
        build.outputs.get("version").map(String::as_str),
        Some("1.2.3"),
        "the GITHUB_OUTPUT value is finalized as a job output"
    );

    // `if: failure()` stays skipped while the job is green; `continue-on-error`
    // keeps the tolerated failure from failing the job.
    let only_on_failure = build
        .steps
        .iter()
        .find(|step| step.label == "only on failure")
        .expect("the if: failure() step is present");
    assert!(!only_on_failure.ran, "the if: failure() step is skipped");
    let soft = build
        .steps
        .iter()
        .find(|step| step.label == "soft failure is tolerated")
        .expect("the continue-on-error step is present");
    assert_eq!(
        soft.outcome,
        Conclusion::Failure,
        "its raw outcome is failure"
    );
    assert_eq!(
        soft.conclusion,
        Conclusion::Success,
        "continue-on-error rescues its conclusion"
    );

    // The dependent job consumed the propagated output and passed.
    let report_job = report
        .jobs
        .iter()
        .find(|job| job.id == "report")
        .expect("report job ran");
    assert_eq!(
        report_job.result,
        Conclusion::Success,
        "needs.build.outputs.version propagated to the dependent\n--- log ---\n{log}"
    );

    // The custom job-container job ran in the requested image.
    let container_job = report
        .jobs
        .iter()
        .find(|job| job.id == "container-job")
        .expect("container job ran");
    assert_eq!(
        container_job.result,
        Conclusion::Success,
        "the custom container job is green\n--- log ---\n{log}"
    );

    // The grouped output was folded and rendered.
    assert!(log.contains("Compiling"), "the group title is shown");
}
