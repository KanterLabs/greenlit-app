//! Real-daemon integration tests: node20 *and* node24 JavaScript actions
//! execute through the checksum-pinned-runtime protocol, in both an
//! ordinary (no `container:`) job and a custom job container
//! (`PHASE-3-actions.md` exit criterion 6).
//!
//! Bundle downloads stay out of the default test path: every test here
//! injects a tiny synthetic `.tar.gz` bundle (a POSIX shell script standing
//! in for `bin/node`) through the same [`RuntimeBundleFetcher`]/
//! [`NodeBundleSpecs`] seam production code drives with the real pinned
//! bundles — this proves the mounting/`INPUT_*`/command-file/exec protocol
//! this crate owns, without needing the real ~100 MiB downloads (which are
//! proven, opt-in-live, in `actions_nodejs_live.rs`). The container engine
//! is a true external, so it is used real here, not faked (`TESTING.md`).

mod dockerkit;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::sync::Arc;

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

/// A fake "node" executable: a POSIX shell script (works under both glibc
/// and Alpine/busybox `sh`) that ignores its script-path argument and
/// proves the env/command-file protocol instead: echoes `INPUT_GREETING`
/// and `GITHUB_ACTION_PATH`, and writes a `GITHUB_OUTPUT` assignment.
const FAKE_NODE_SCRIPT: &str = "#!/bin/sh\n\
echo \"fake-node ran: input=$INPUT_GREETING action_path=$GITHUB_ACTION_PATH\"\n\
echo \"greeted=$INPUT_GREETING\" >> \"$GITHUB_OUTPUT\"\n";

/// Builds a tiny gzip-compressed tar containing `bin/node` (the fake script
/// above), mirroring the real bundles' `bin/node` layout.
fn fake_node_bundle_bytes() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_path("bin/node").unwrap();
    header.set_size(FAKE_NODE_SCRIPT.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append(&header, FAKE_NODE_SCRIPT.as_bytes())
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

/// Serves the same tiny fake bundle for every `(version, variant)` asked
/// for, counting calls.
struct FakeBundleFetcher {
    bytes: Vec<u8>,
}

#[async_trait::async_trait]
impl RuntimeBundleFetcher for FakeBundleFetcher {
    async fn download(&self, _spec: &RuntimeBundleSpec) -> Result<Vec<u8>, RuntimeBundleError> {
        Ok(self.bytes.clone())
    }
}

/// Names the *fake* bundle's own checksum instead of the real pinned one —
/// the seam that makes a tiny synthetic bundle checksum-verifiable at all
/// (`NodeBundleSpecs` module docs).
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

fn fake_action_config(store_root: &std::path::Path) -> greenlit_runtime::ActionRuntimeConfig {
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
        resolver: Arc::new(NeverResolves),
        store: ActionStore::at(store_root.join("actions")),
        fetcher: Arc::new(NeverFetches),
        node_runtime_fetcher: Arc::new(FakeBundleFetcher { bytes }),
        node_runtime_specs: Arc::new(FakeBundleSpecs { sha256 }),
        node_runtime_store: RuntimeStore::at(store_root.join("node-runtimes")),
        github_token: None,
    }
}

/// Writes a local Node action (`action.yml` + a placeholder `main.js`, never
/// actually read by the fake `bin/node` script) at
/// `<repo_root>/.github/actions/<name>/`.
fn write_local_node_action(repo_root: &std::path::Path, name: &str, using: &str) {
    let dir = repo_root.join(".github/actions").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("action.yml"),
        format!(
            "name: {name}\ninputs:\n  greeting:\n    default: hello\nruns:\n  using: {using}\n  main: main.js\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("main.js"),
        b"// never executed by the fake node runtime\n",
    )
    .unwrap();
}

fn synthetic_push_event() -> SyntheticEvent {
    let github = Value::object(vec![
        ("event_name".to_string(), Value::String("push".to_string())),
        (
            "repository".to_string(),
            Value::String("greenlit/actions-nodejs".to_string()),
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
        workflow: "actions-nodejs".to_string(),
        repository: "greenlit/actions-nodejs".to_string(),
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
async fn node20_and_node24_actions_execute_in_the_ordinary_job_container() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("node20_and_node24_actions_execute_in_the_ordinary_job_container");
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    write_local_node_action(repo_root.path(), "fake-node20", "node20");
    write_local_node_action(repo_root.path(), "fake-node24", "node24");
    std::fs::create_dir_all(repo_root.path().join(".github/workflows")).unwrap();
    std::fs::write(
        repo_root.path().join(".github/workflows/ci.yml"),
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - id: n20\n        uses: ./.github/actions/fake-node20\n        with:\n          greeting: hi-twenty\n      - id: n24\n        uses: ./.github/actions/fake-node24\n        with:\n          greeting: hi-twentyfour\n",
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
    let workspace = "/home/runner/work/actions-nodejs/actions-nodejs".to_string();
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
        volume_namespace: "actions-nodejs-ordinary".to_string(),
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: fake_action_config(store_root.path()),
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
        "both actions succeed\n--- log ---\n{log}"
    );
    let build = &report.jobs[0];
    assert_eq!(build.result, Conclusion::Success);
    assert!(
        log.contains("input=hi-twenty") && log.contains("input=hi-twentyfour"),
        "both node20 and node24 actions received their INPUT_* env var\n--- log ---\n{log}"
    );
    assert!(
        log.contains(".github/actions/fake-node20") && log.contains(".github/actions/fake-node24"),
        "GITHUB_ACTION_PATH pointed at each action's own directory\n--- log ---\n{log}"
    );
}

#[tokio::test]
async fn a_node_action_executes_in_a_custom_job_container() {
    let Some(engine) = engine_if_reachable().await else {
        notice_no_daemon("a_node_action_executes_in_a_custom_job_container");
        return;
    };

    let repo_root = tempfile::tempdir().unwrap();
    write_local_node_action(repo_root.path(), "fake-node20", "node20");
    std::fs::create_dir_all(repo_root.path().join(".github/workflows")).unwrap();
    std::fs::write(
        repo_root.path().join(".github/workflows/ci.yml"),
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    container:\n      image: ubuntu:24.04\n    steps:\n      - uses: ./.github/actions/fake-node20\n        with:\n          greeting: hi-from-container\n",
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
    let workspace = "/home/runner/work/actions-nodejs/actions-nodejs".to_string();
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
        volume_namespace: "actions-nodejs-custom".to_string(),
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: fake_action_config(store_root.path()),
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
        "the action succeeds inside the custom job container\n--- log ---\n{log}"
    );
    assert!(
        log.contains("input=hi-from-container"),
        "the action ran inside the requested container image\n--- log ---\n{log}"
    );
}
