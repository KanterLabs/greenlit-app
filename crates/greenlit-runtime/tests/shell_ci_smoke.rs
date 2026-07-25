//! Real-daemon smoke test: the `fixtures/shell-ci` workflow runs green end to
//! end through the executor (`PHASE-2-execution.md` exit criterion 1).
//!
//! This exercises the whole Phase 2 path against a live engine: base and custom
//! job containers, overlay/copy-in isolation, shell resolution, env layering,
//! `GITHUB_OUTPUT`/`GITHUB_ENV` command files, a `::group::` block,
//! `continue-on-error`, `if: failure()`, and a declared job output propagated to
//! a direct `needs` dependent. The engine is a true external, so it is used real
//! here, not faked (`TESTING.md`).
//!
//! Also covers the runtime-supplied `github` context roots and env vars this
//! crate fills in at execution time rather than plan time: `GITHUB_EVENT_PATH`
//! and the file it points at, `${{ github.event_path }}`/`workspace`/`job`,
//! and per-step `GITHUB_ACTION` (id-less run steps deduping to `__run`/
//! `__run_2`, an explicit `id:` winning outright).

mod dockerkit;

use std::collections::BTreeSet;
use std::path::PathBuf;

use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::{
    Conclusion, EventKind, PlanOptions, SyntheticEvent, plan, validate_v0_support,
};
use greenlit_expr::Value;
use greenlit_runtime::{IsolationStrategy, ProgressNull, RunConfig, run_plan};

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
        // The `github.event` payload the executor serializes to
        // `GITHUB_EVENT_PATH` (`crate::executor::event_json`); only the
        // field this file's own assertion greps for is populated, matching
        // this test's existing minimal-fixture style.
        (
            "event".to_string(),
            Value::object(vec![(
                "repository".to_string(),
                Value::object(vec![(
                    "full_name".to_string(),
                    Value::String("greenlit/shell-ci".to_string()),
                )]),
            )]),
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
        actions_service: None,
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
        locked_images: None,
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: dockerkit::test_action_config(),
        store: None,
        resources: greenlit_runtime::ResourceLimits::default(),
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

    // `GITHUB_EVENT_PATH` points at a real file carrying the serialized
    // `github.event` payload (`crate::executor::event_json`/
    // `crate::executor::cmdfiles::write_event_file`), and `github.event_path`/
    // `github.workspace`/`github.job` all resolve at runtime rather than
    // staying deferred (`crate::executor::context::job_github_context`).
    assert!(
        log.contains("/event.json|"),
        "github.event_path resolves to the written event file\n--- log ---\n{log}"
    );
    assert!(
        log.contains(&format!("|{workspace}|build]")),
        "github.workspace/job interpolate to this job's own values\n--- log ---\n{log}"
    );

    // Per-step `GITHUB_ACTION` (`crate::executor::step_ids`): an id-less
    // `run:` step gets the runner's own default `__run`, a repeated one
    // dedups to `__run_2`, and an explicit `id:` wins outright.
    let action_ids_job = report
        .jobs
        .iter()
        .find(|job| job.id == "action-ids")
        .expect("action-ids job ran");
    assert_eq!(
        action_ids_job.result,
        Conclusion::Success,
        "action-ids job is green\n--- log ---\n{log}"
    );
    assert!(
        log.contains("GITHUB_ACTION_IS:[__run]"),
        "the first id-less run step's GITHUB_ACTION is __run\n--- log ---\n{log}"
    );
    assert!(
        log.contains("GITHUB_ACTION_IS:[__run_2]"),
        "the second id-less run step dedups to __run_2\n--- log ---\n{log}"
    );
    assert!(
        log.contains("GITHUB_ACTION_IS:[explicit]"),
        "an explicit id: wins outright over any generated candidate\n--- log ---\n{log}"
    );
}
