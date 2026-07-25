//! Opt-in live test: a real node20 action executes using the *real*,
//! checksum-pinned `actions/runner` Node runtime bundle downloaded from its
//! real pinned URL (`PHASE-3-actions.md` exit criterion 6: "the real-bundle
//! path proven in the opt-in live test").
//!
//! Gated behind `LITCI_TEST_LIVE_NODE_DOWNLOAD=1` so the default `cargo
//! test` path never downloads the real ~100 MiB bundle
//! (`PHASE-3-actions.md`: "keep bundle downloads out of the default path").
//! Run explicitly with:
//!
//! ```text
//! LITCI_TEST_LIVE_NODE_DOWNLOAD=1 cargo test -p greenlit-runtime --test actions_nodejs_live -- --ignored
//! ```
//!
//! (also requires a reachable Docker daemon and internet access to
//! `nodejs.org`). Downloaded bundles land in a temp directory removed at
//! the end of the test, never `~/.litci`.

mod dockerkit;

use std::collections::BTreeSet;
use std::sync::Arc;

use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::{Conclusion, EventKind, PlanOptions, SyntheticEvent, plan};
use greenlit_expr::Value;
use greenlit_runtime::executor::actions::node_runtime::{
    HttpRuntimeBundleFetcher, PinnedNodeBundleSpecs, RuntimeStore,
};
use greenlit_runtime::{IsolationStrategy, ProgressNull, RunConfig, run_plan};

use dockerkit::{engine_if_reachable, notice_no_daemon};

const LIVE_ENV_VAR: &str = "LITCI_TEST_LIVE_NODE_DOWNLOAD";

fn real_action_config(store_root: &std::path::Path) -> greenlit_runtime::ActionRuntimeConfig {
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
                message: "not used: this test only references a local action".to_string(),
            })
        }
    }

    greenlit_runtime::ActionRuntimeConfig {
        resolver: Arc::new(NeverResolves),
        store: ActionStore::at(store_root.join("actions")),
        fetcher: Arc::new(NeverFetches),
        // The real HTTPS fetcher and the real pinned checksums — the one
        // thing this test exists to exercise.
        node_runtime_fetcher: Arc::new(HttpRuntimeBundleFetcher::new()),
        node_runtime_specs: Arc::new(PinnedNodeBundleSpecs),
        node_runtime_store: RuntimeStore::at(store_root.join("node-runtimes")),
        github_token: None,
    }
}

fn synthetic_push_event() -> SyntheticEvent {
    let github = Value::object(vec![
        ("event_name".to_string(), Value::String("push".to_string())),
        (
            "repository".to_string(),
            Value::String("greenlit/actions-nodejs-live".to_string()),
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
        workflow: "actions-nodejs-live".to_string(),
        repository: "greenlit/actions-nodejs-live".to_string(),
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
async fn a_real_node20_action_runs_with_the_real_pinned_bundle() {
    if std::env::var_os(LIVE_ENV_VAR).is_none() {
        eprintln!(
            "a_real_node20_action_runs_with_the_real_pinned_bundle: skipped (set {LIVE_ENV_VAR}=1 to download the real ~100 MiB pinned Node runtime bundle and run this live)"
        );
        return;
    }
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("a_real_node20_action_runs_with_the_real_pinned_bundle");
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    let action_dir = repo_root.path().join(".github/actions/real-node20");
    std::fs::create_dir_all(&action_dir).unwrap();
    std::fs::write(
        action_dir.join("action.yml"),
        "name: real node20 fixture\ninputs:\n  greeting:\n    default: hello\nruns:\n  using: node20\n  main: main.js\n",
    )
    .unwrap();
    // A genuine Node.js script (not a shell-script stand-in): proves the
    // *real* pinned node20 binary actually runs JavaScript, reads
    // `INPUT_*`, and writes `GITHUB_OUTPUT`.
    std::fs::write(
        action_dir.join("main.js"),
        "const fs = require('fs');\n\
         console.log('real-node20 saw: ' + process.env.INPUT_GREETING);\n\
         fs.appendFileSync(process.env.GITHUB_OUTPUT, 'greeted=' + process.env.INPUT_GREETING + '\\n');\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo_root.path().join(".github/workflows")).unwrap();
    std::fs::write(
        repo_root.path().join(".github/workflows/ci.yml"),
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: ./.github/actions/real-node20\n        with:\n          greeting: live-hello\n",
    )
    .unwrap();

    let workflow = greenlit_workflow::parse_workflow_file_with_name(
        repo_root.path().join(".github/workflows/ci.yml"),
        "ci.yml",
    )
    .expect("parse");
    let event = synthetic_push_event();
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let store_root = tempfile::tempdir().unwrap();
    let workspace = "/home/runner/work/actions-nodejs-live/actions-nodejs-live".to_string();
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
        volume_namespace: "actions-nodejs-live".to_string(),
        locked_images: None,
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: real_action_config(store_root.path()),
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
        "the real node20 bundle executes the action\n--- log ---\n{log}"
    );
    assert!(
        log.contains("real-node20 saw: live-hello"),
        "the real pinned node20 binary ran the action's JavaScript\n--- log ---\n{log}"
    );
}
