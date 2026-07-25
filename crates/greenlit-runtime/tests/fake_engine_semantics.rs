//! Executor orchestration semantics against a fake, scripted [`ContainerEngine`]
//! — no Docker required.
//!
//! The container engine is a true external, so it is the one thing faked here
//! (`TESTING.md`: "Mock only true externals ... the engine"). This scripted fake
//! interprets a tiny directive language embedded in the workflow's `run:`
//! bodies, so the test exercises exactly the executor's *orchestration*: needs
//! output propagation, the `outcome`/`conclusion` roll-up, skip gating across a
//! DAG, and live secret masking — the semantics that do not need a real shell.
//! GitHub-faithful shell/env/command-file behavior is proven separately by the
//! real-daemon `shell_ci_smoke` test.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::{Conclusion, EventKind, PlanOptions, SyntheticEvent, plan};
use greenlit_expr::Value;
use greenlit_runtime::engine::{
    BuildSpec, CommitSpec, ContainerEngine, ContainerSpec, ContainerState, ExecOutput,
    ExecOutputSink, ExecSpec, HealthState, ImageSummary, NetworkInfo, RegistryAuth,
};
use greenlit_runtime::error::{Operation, RuntimeError};
use greenlit_runtime::progress::{ProgressEvent, ProgressNull, ProgressSink, WorkspaceProgress};
use greenlit_runtime::{IsolationStrategy, ReadinessConfig, RunConfig, run_plan};

/// How the fake engine answers the executor's readiness probe.
#[derive(Default)]
enum ReadinessScript {
    /// The marker exists on the first poll (the pre-existing behavior).
    #[default]
    ImmediatelyReady,
    /// Serve each status text once, then report ready alongside the last.
    StatusSequenceThenReady(Vec<&'static str>),
    /// Serve the same status text forever — drives the inactivity timeout.
    StatusFrozen(&'static str),
    /// The probe exec itself fails — drives the missing-`sh` classification.
    ProbeExecErrors,
}

/// A fake engine that keeps an in-memory file map and interprets a directive
/// language in the stored step scripts: `OUT k=v` (→ GITHUB_OUTPUT), `ENV k=v`
/// (→ GITHUB_ENV), `ECHO text` (→ stdout), and `EXIT n` (exit code).
#[derive(Default)]
struct ScriptedEngine {
    files: Mutex<HashMap<String, String>>,
    counter: AtomicUsize,
    /// When set, `image_exists` reports absence so the executor drives the
    /// build path (and its progress events).
    image_absent: bool,
    /// Scripted readiness-probe behavior.
    readiness: ReadinessScript,
    /// How many readiness polls have been answered.
    polls: AtomicUsize,
    created_images: Mutex<Vec<String>>,
}

impl ScriptedEngine {
    fn store(&self, path: String, content: String) {
        self.files.lock().unwrap().insert(path, content);
    }

    fn read(&self, path: &str) -> String {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    fn append(&self, path: &str, text: &str) {
        let mut files = self.files.lock().unwrap();
        files.entry(path.to_string()).or_default().push_str(text);
    }

    /// Parse the preparing exec: truncate the four command files and store the
    /// heredoc-written script.
    fn handle_prepare(&self, program: &str) {
        for part in program.split("&&") {
            if let Some(rest) = part.trim().strip_prefix(": > ")
                && let Some(path) = rest.split_whitespace().next()
            {
                self.store(path.to_string(), String::new());
            }
        }
        if let Some(cat) = program.find("cat > ") {
            let after = &program[cat + 6..];
            if let Some(sp_end) = after.find(" <<'") {
                let script_path = after[..sp_end].to_string();
                let after_delim = &after[sp_end + 4..];
                if let Some(delim_end) = after_delim.find("'\n") {
                    let delim = &after_delim[..delim_end];
                    let body_region = &after_delim[delim_end + 2..];
                    let end = format!("\n{delim}\n");
                    let body = body_region
                        .split(&end)
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    self.store(script_path, body);
                }
            }
        }
    }

    /// Interpret a stored step script, emitting output and writing command files.
    fn interpret(
        &self,
        script: &str,
        env: &[(String, String)],
        sink: &mut dyn ExecOutputSink,
    ) -> i64 {
        let env: HashMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut exit = 0;
        for raw in script.lines() {
            let line = raw.trim();
            if let Some(rest) = line.strip_prefix("OUT ") {
                if let Some(path) = env.get("GITHUB_OUTPUT") {
                    self.append(path, &format!("{rest}\n"));
                }
            } else if let Some(rest) = line.strip_prefix("ENV ") {
                if let Some(path) = env.get("GITHUB_ENV") {
                    self.append(path, &format!("{rest}\n"));
                }
            } else if let Some(rest) = line.strip_prefix("ECHO ") {
                sink.on_stdout(format!("{rest}\n").as_bytes());
            } else if let Some(rest) = line.strip_prefix("EXIT ") {
                exit = rest.trim().parse().unwrap_or(1);
            }
        }
        exit
    }
}

#[async_trait]
impl ContainerEngine for ScriptedEngine {
    async fn pull_image(
        &self,
        _image: &str,
        _auth: Option<&RegistryAuth>,
        _progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn image_exists(&self, _image: &str) -> Result<bool, RuntimeError> {
        Ok(!self.image_absent)
    }
    async fn build_image(
        &self,
        spec: &BuildSpec,
        progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError> {
        // Mirror the real engine's contract: engines report their own
        // build progress.
        progress.on_progress(ProgressEvent::BuildStarted {
            tag: spec.tag.clone(),
        });
        progress.on_progress(ProgressEvent::BuildFinished {
            tag: spec.tag.clone(),
        });
        Ok(())
    }
    async fn commit_container(&self, _spec: &CommitSpec) -> Result<String, RuntimeError> {
        Ok("committed".to_string())
    }
    async fn create_container(&self, spec: &ContainerSpec) -> Result<String, RuntimeError> {
        self.created_images.lock().unwrap().push(spec.image.clone());
        Ok(format!(
            "fake-{}",
            self.counter.fetch_add(1, Ordering::Relaxed)
        ))
    }
    async fn start_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn stop_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn remove_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn exec(
        &self,
        _container: &str,
        spec: &ExecSpec,
        sink: &mut (dyn ExecOutputSink + Send),
    ) -> Result<ExecOutput, RuntimeError> {
        let cmd = &spec.cmd;
        // Readiness poll: the probe prints GREENLIT_READY when the marker
        // exists, then the status file's contents.
        if cmd.len() == 3 && cmd[0] == "sh" && cmd[2].contains("/greenlit/ready") {
            let poll = self.polls.fetch_add(1, Ordering::Relaxed);
            match &self.readiness {
                ReadinessScript::ImmediatelyReady => {
                    sink.on_stdout(b"GREENLIT_READY\n");
                }
                ReadinessScript::StatusSequenceThenReady(sequence) => {
                    if let Some(status) = sequence.get(poll) {
                        sink.on_stdout(status.as_bytes());
                    } else {
                        let last = sequence.last().copied().unwrap_or("");
                        sink.on_stdout(format!("GREENLIT_READY\n{last}").as_bytes());
                    }
                }
                ReadinessScript::StatusFrozen(status) => {
                    sink.on_stdout(status.as_bytes());
                }
                ReadinessScript::ProbeExecErrors => {
                    return Err(RuntimeError::Api {
                        operation: Operation::StartExec,
                        source: bollard::errors::Error::IOError {
                            err: std::io::Error::other("no sh in this image"),
                        },
                    });
                }
            }
            return Ok(ExecOutput { exit_code: 0 });
        }
        // Command-file preparation + script write.
        if cmd.len() == 3 && cmd[0] == "sh" && cmd[2].contains("cat > ") {
            self.handle_prepare(&cmd[2]);
            return Ok(ExecOutput { exit_code: 0 });
        }
        // A command-file read-back.
        if cmd.len() == 2 && cmd[0] == "cat" {
            let content = self.read(&cmd[1]);
            sink.on_stdout(content.as_bytes());
            return Ok(ExecOutput { exit_code: 0 });
        }
        // Otherwise it is a step exec: find the stored script among the args.
        let script_path = cmd
            .iter()
            .find(|arg| self.files.lock().unwrap().contains_key(*arg));
        let exit = match script_path {
            Some(path) => {
                let script = self.read(path);
                self.interpret(&script, &spec.env, sink)
            }
            None => 0,
        };
        Ok(ExecOutput { exit_code: exit })
    }
    async fn run_container(
        &self,
        _id: &str,
        _sink: &mut (dyn ExecOutputSink + Send),
    ) -> Result<ExecOutput, RuntimeError> {
        Ok(ExecOutput { exit_code: 0 })
    }
    async fn export_path(&self, _container: &str, _path: &str) -> Result<Vec<u8>, RuntimeError> {
        Ok(Vec::new())
    }
    async fn create_network(&self, _name: &str) -> Result<String, RuntimeError> {
        Ok("net".to_string())
    }
    async fn remove_network(&self, _name: &str) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn inspect_network(&self, _name: &str) -> Result<NetworkInfo, RuntimeError> {
        Ok(NetworkInfo {
            gateway: Some("10.0.0.1".to_string()),
            subnet: Some("10.0.0.0/16".to_string()),
        })
    }
    async fn inspect_container(&self, _id: &str) -> Result<ContainerState, RuntimeError> {
        Ok(ContainerState {
            running: true,
            exit_code: None,
            health: HealthState::None,
        })
    }
    async fn create_volume(&self, _name: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn remove_volume(&self, _name: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn list_images(&self, _label: &str) -> Result<Vec<ImageSummary>, RuntimeError> {
        Ok(Vec::new())
    }
    async fn remove_image(&self, _image: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn execution_uses_locked_image_identity_instead_of_mutable_alias() {
    let workflow = greenlit_workflow::parse_workflow(
        "locked-image.yml",
        r#"
on: push
jobs:
  only:
    runs-on: ubuntu-latest
    container:
      image: mutable.example/tool:latest
    steps:
      - run: ECHO locked
"#,
    )
    .expect("parse");
    let event = SyntheticEvent {
        kind: EventKind::Push,
        github: Value::object(vec![(
            "event_name".to_string(),
            Value::String("push".to_string()),
        )]),
        inputs: Value::object(vec![]),
        deferred_github_properties: std::collections::BTreeSet::new(),
    };
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");
    let engine = ScriptedEngine::default();
    let config = RunConfig {
        repo_host_path: std::env::temp_dir(),
        workspace: "/ws".to_string(),
        strategy: IsolationStrategy::Auto,
        runner_env: RunnerEnv::default(),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "locked-image".to_string(),
        locked_images: Some(std::collections::BTreeMap::from([
            (
                "mutable.example/tool:latest".to_string(),
                "sha256:immutable".to_string(),
            ),
            ("alpine:3.20".to_string(), "sha256:netguard".to_string()),
        ])),
        write_back: false,
        readiness: ReadinessConfig::default(),
        actions: test_action_config(),
        store: None,
    };

    run_plan(
        &engine,
        &execution_plan,
        &config,
        &mut Vec::new(),
        &mut ProgressNull,
    )
    .await
    .expect("run completes");
    let created = engine.created_images.lock().unwrap();
    assert_eq!(
        created.first().map(String::as_str),
        Some("sha256:immutable")
    );
    assert!(
        !created
            .iter()
            .any(|image| image == "mutable.example/tool:latest"),
        "a mutable alias must never cross the container-engine boundary after locking: {created:?}"
    );
    assert!(created.iter().any(|image| image == "sha256:netguard"));
    assert!(!created.iter().any(|image| image == "alpine:3.20"));
}

const WORKFLOW: &str = r#"
name: fake-ci
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    env:
      WHO: alice
    outputs:
      token: ${{ steps.s.outputs.token }}
      via_env: ${{ env.WHO }}
      via_github_env: ${{ env.STAMP }}
    steps:
      - id: s
        run: |
          OUT token=abc
      - name: write env
        run: |
          ENV STAMP=stamped
      - name: masking
        run: |
          ECHO ::add-mask::topsecret
          ECHO leaking topsecret
  b:
    runs-on: ubuntu-latest
    needs: a
    steps:
      - name: consume
        run: |
          ECHO got ${{ needs.a.outputs.token }}
      - name: boom
        run: |
          EXIT 1
      - name: on failure
        if: failure()
        run: |
          ECHO recovered
      - name: after failure
        if: success()
        run: |
          ECHO should not run
  c:
    runs-on: ubuntu-latest
    needs: b
    steps:
      - name: c step
        run: |
          ECHO c ran
"#;

#[tokio::test]
async fn dag_propagation_rollup_gating_and_masking() {
    let workflow = greenlit_workflow::parse_workflow("fake-ci.yml", WORKFLOW).expect("parse");
    let event = SyntheticEvent {
        kind: EventKind::Push,
        github: Value::object(vec![(
            "event_name".to_string(),
            Value::String("push".to_string()),
        )]),
        inputs: Value::object(vec![]),
        deferred_github_properties: std::collections::BTreeSet::new(),
    };
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let engine = ScriptedEngine::default();
    let config = RunConfig {
        repo_host_path: std::env::temp_dir(),
        workspace: "/ws".to_string(),
        strategy: IsolationStrategy::Auto,
        runner_env: RunnerEnv::default(),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "fake-engine-semantics".to_string(),
        locked_images: None,
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: test_action_config(),
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
    let log = String::from_utf8(log).unwrap();

    assert_eq!(
        report.overall,
        Conclusion::Failure,
        "b fails, so the run fails"
    );

    let a = report.jobs.iter().find(|j| j.id == "a").unwrap();
    assert_eq!(a.result, Conclusion::Success);
    assert_eq!(a.outputs.get("token").map(String::as_str), Some("abc"));
    // Job outputs evaluate against the full job env: workflow/job `env:` and
    // values written to `GITHUB_ENV` by earlier steps are both in scope.
    assert_eq!(a.outputs.get("via_env").map(String::as_str), Some("alice"));
    assert_eq!(
        a.outputs.get("via_github_env").map(String::as_str),
        Some("stamped")
    );

    // Deferred `needs.a.outputs.token` finished against the live needs context.
    assert!(log.contains("got abc"), "needs output propagated: {log}");

    // Masking: the add-mask line never prints and later echoes are redacted.
    assert!(log.contains("leaking ***"), "secret redacted: {log}");
    assert!(!log.contains("topsecret"), "secret never leaks: {log}");

    let b = report.jobs.iter().find(|j| j.id == "b").unwrap();
    assert_eq!(b.result, Conclusion::Failure);
    let on_failure = b.steps.iter().find(|s| s.label == "on failure").unwrap();
    assert!(on_failure.ran, "if: failure() runs after the failure");
    let after = b.steps.iter().find(|s| s.label == "after failure").unwrap();
    assert!(!after.ran, "if: success() is skipped after the failure");

    // A dependent of a failed job is skipped, not run.
    let c = report.jobs.iter().find(|j| j.id == "c").unwrap();
    assert_eq!(c.result, Conclusion::Skipped);
    assert!(!log.contains("c ran"), "the skipped job's step never ran");
}

/// `PHASE-3-actions.md` exit criterion 3: "checkout's post step runs even
/// when a later step fails." `actions/checkout` is self-satisfied (no
/// network, matching the current repository — `RunnerEnv::default()`'s
/// empty `repository` and this step's own absent `with.repository` are
/// equal, so this is the self-checkout path) and its post step is a
/// documented no-op, but it must still be *registered and drained*: the
/// job's step-loop failure must not skip the post phase.
const CHECKOUT_THEN_FAILING_STEP_WORKFLOW: &str = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          EXIT 1
"#;

#[tokio::test]
async fn checkouts_post_step_runs_even_when_a_later_step_fails() {
    let workflow =
        greenlit_workflow::parse_workflow("fake-ci.yml", CHECKOUT_THEN_FAILING_STEP_WORKFLOW)
            .expect("parse");
    let event = SyntheticEvent {
        kind: EventKind::Push,
        github: Value::object(vec![(
            "event_name".to_string(),
            Value::String("push".to_string()),
        )]),
        inputs: Value::object(vec![]),
        deferred_github_properties: std::collections::BTreeSet::new(),
    };
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");
    greenlit_runtime::reject_uses_steps(&execution_plan)
        .expect("a well-formed uses: step passes preflight");

    let engine = ScriptedEngine::default();
    let config = RunConfig {
        repo_host_path: std::env::temp_dir(),
        workspace: "/ws".to_string(),
        strategy: IsolationStrategy::Auto,
        runner_env: RunnerEnv::default(),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "fake-engine-semantics".to_string(),
        locked_images: None,
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: test_action_config(),
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
    .expect("the run completes (a failed step is a recorded result, not an error)");

    let job = &report.jobs[0];
    assert_eq!(
        job.result,
        Conclusion::Failure,
        "the later step's failure fails the job: {job:?}"
    );
    let post_step = job
        .steps
        .iter()
        .find(|step| step.label == "Post actions/checkout@v4")
        .unwrap_or_else(|| panic!("checkout's post step must appear in the report: {job:?}"));
    assert!(
        post_step.ran,
        "checkout's post step must run despite the job's failure: {job:?}"
    );
    assert_eq!(post_step.conclusion, Conclusion::Success);
    // Post steps drain after every main step, regardless of outcome — the
    // post entry is therefore last in the report's step order.
    assert_eq!(
        job.steps.last().map(|step| step.label.as_str()),
        Some("Post actions/checkout@v4"),
        "post steps run at job end: {job:?}"
    );
}

/// A sink that records event discriminants in arrival order.
#[derive(Default)]
struct RecordingSink {
    events: Vec<String>,
}

impl ProgressSink for RecordingSink {
    fn on_progress(&mut self, event: ProgressEvent) {
        let name = match event {
            ProgressEvent::PullStarted { .. } => "pull-started",
            ProgressEvent::PullProgress { .. } => "pull-progress",
            ProgressEvent::PullFinished { .. } => "pull-finished",
            ProgressEvent::BuildStarted { .. } => "build-started",
            ProgressEvent::BuildLine { .. } => "build-line",
            ProgressEvent::BuildFinished { .. } => "build-finished",
            ProgressEvent::BootStarted => "boot-started",
            ProgressEvent::BootFinished => "boot-finished",
            ProgressEvent::Workspace(WorkspaceProgress::FellBack { .. }) => "workspace-fellback",
            ProgressEvent::Workspace(WorkspaceProgress::Copying { .. }) => "workspace-copying",
            ProgressEvent::Workspace(WorkspaceProgress::Ready { .. }) => "workspace-ready",
            ProgressEvent::Workspace(_) => "workspace",
            _ => "other",
        };
        self.events.push(name.to_string());
    }
}

const SINGLE_JOB_WORKFLOW: &str = r#"
on: push
jobs:
  only:
    runs-on: ubuntu-latest
    steps:
      - run: |
          ECHO hi
"#;

#[tokio::test]
async fn preparation_progress_events_arrive_in_phase_order() {
    let workflow =
        greenlit_workflow::parse_workflow("fake-ci.yml", SINGLE_JOB_WORKFLOW).expect("parse");
    let event = SyntheticEvent {
        kind: EventKind::Push,
        github: Value::object(vec![(
            "event_name".to_string(),
            Value::String("push".to_string()),
        )]),
        inputs: Value::object(vec![]),
        deferred_github_properties: std::collections::BTreeSet::new(),
    };
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");

    let engine = ScriptedEngine::default();
    let config = RunConfig {
        repo_host_path: std::env::temp_dir(),
        workspace: "/ws".to_string(),
        strategy: IsolationStrategy::Auto,
        runner_env: RunnerEnv::default(),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "fake-engine-semantics".to_string(),
        locked_images: None,
        write_back: false,
        readiness: greenlit_runtime::ReadinessConfig::default(),
        actions: test_action_config(),
        store: None,
    };

    let mut log: Vec<u8> = Vec::new();
    let mut recording = RecordingSink::default();
    run_plan(&engine, &execution_plan, &config, &mut log, &mut recording)
        .await
        .expect("run completes");

    assert_eq!(
        recording.events,
        vec!["boot-started", "boot-finished", "workspace-ready"],
        "the already-locked runner profile proceeds through boot and workspace readiness in order"
    );
}

/// A minimal `ActionRuntimeConfig`: it resolves only the built-in checkout
/// action identity and fails every source fetch or other action resolution.
fn test_action_config() -> greenlit_runtime::executor::actions::ActionRuntimeConfig {
    use greenlit_actions::CommitSha;
    use greenlit_actions::resolve::{RefResolver, ResolveError};
    use greenlit_actions::store::{ActionFetcher, ActionStore, FetchError};
    use greenlit_runtime::executor::actions::node_runtime::{
        PinnedNodeBundleSpecs, RuntimeBundleError, RuntimeBundleFetcher, RuntimeBundleSpec,
        RuntimeStore,
    };
    use std::path::Path;
    use std::sync::Arc;

    struct CheckoutOnlyResolver;
    #[async_trait::async_trait]
    impl RefResolver for CheckoutOnlyResolver {
        async fn resolve(
            &self,
            owner: &str,
            repo: &str,
            git_ref: &str,
        ) -> Result<CommitSha, ResolveError> {
            if owner == "actions" && repo == "checkout" {
                return CommitSha::parse(&"c".repeat(40)).map_err(|error| {
                    ResolveError::TaskFailed {
                        owner: owner.to_string(),
                        repo: repo.to_string(),
                        git_ref: git_ref.to_string(),
                        message: error.to_string(),
                    }
                });
            }
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
            _dest: &Path,
        ) -> Result<(), FetchError> {
            Err(FetchError::Download {
                owner: owner.to_string(),
                repo: repo.to_string(),
                sha: sha.as_str().to_string(),
                message: "not available in this test".to_string(),
            })
        }
    }
    struct NeverDownloads;
    #[async_trait::async_trait]
    impl RuntimeBundleFetcher for NeverDownloads {
        async fn download(&self, spec: &RuntimeBundleSpec) -> Result<Vec<u8>, RuntimeBundleError> {
            Err(RuntimeBundleError::Download {
                url: spec.url.to_string(),
                message: "not available in this test".to_string(),
            })
        }
    }

    greenlit_runtime::executor::actions::ActionRuntimeConfig {
        resolver: Arc::new(CheckoutOnlyResolver),
        store: ActionStore::at(std::env::temp_dir().join("greenlit-test-unused-action-store")),
        fetcher: Arc::new(NeverFetches),
        node_runtime_fetcher: Arc::new(NeverDownloads),
        node_runtime_specs: Arc::new(PinnedNodeBundleSpecs),
        node_runtime_store: RuntimeStore::at(
            std::env::temp_dir().join("greenlit-test-unused-node-store"),
        ),
        github_token: None,
    }
}

/// Deadlines small enough that timeout scenarios resolve in tens of
/// milliseconds instead of the production seconds.
fn tiny_readiness() -> ReadinessConfig {
    ReadinessConfig {
        poll_interval: std::time::Duration::from_millis(5),
        first_signal_timeout: std::time::Duration::from_millis(80),
        inactivity_timeout: std::time::Duration::from_millis(80),
    }
}

/// Run [`SINGLE_JOB_WORKFLOW`] against `engine`, returning the run result
/// and the recorded progress-event names.
async fn run_single_job(
    engine: ScriptedEngine,
) -> (
    Result<greenlit_runtime::RunReport, greenlit_runtime::ExecError>,
    Vec<String>,
) {
    let workflow =
        greenlit_workflow::parse_workflow("fake-ci.yml", SINGLE_JOB_WORKFLOW).expect("parse");
    let event = SyntheticEvent {
        kind: EventKind::Push,
        github: Value::object(vec![(
            "event_name".to_string(),
            Value::String("push".to_string()),
        )]),
        inputs: Value::object(vec![]),
        deferred_github_properties: std::collections::BTreeSet::new(),
    };
    let execution_plan = plan(&workflow, &event, &PlanOptions::default()).expect("plan");
    let config = RunConfig {
        repo_host_path: std::env::temp_dir(),
        workspace: "/ws".to_string(),
        strategy: IsolationStrategy::Auto,
        runner_env: RunnerEnv::default(),
        github: event.github.clone(),
        vars: Value::object(vec![]),
        inputs: Value::object(vec![]),
        secrets: Value::object(vec![]),
        initial_masks: Vec::new(),
        volume_namespace: "fake-engine-semantics".to_string(),
        locked_images: None,
        write_back: false,
        readiness: tiny_readiness(),
        actions: test_action_config(),
        store: None,
    };
    let mut log: Vec<u8> = Vec::new();
    let mut recording = RecordingSink::default();
    let result = run_plan(&engine, &execution_plan, &config, &mut log, &mut recording).await;
    (result, recording.events)
}

#[tokio::test]
async fn a_progressing_copy_reports_fallback_once_then_ticks_then_ready() {
    let engine = ScriptedEngine {
        readiness: ReadinessScript::StatusSequenceThenReady(vec![
            "v=1 phase=copy strategy=copy-in fell_back=1 reason=EPERM files=100 bytes=1000",
            "v=1 phase=copy strategy=copy-in fell_back=1 reason=EPERM files=200 bytes=2000",
        ]),
        ..ScriptedEngine::default()
    };
    let (result, events) = run_single_job(engine).await;
    result.expect("a progressing copy completes green");
    let workspace: Vec<&str> = events
        .iter()
        .map(String::as_str)
        .filter(|name| name.starts_with("workspace"))
        .collect();
    assert_eq!(
        workspace,
        vec![
            "workspace-fellback",
            "workspace-copying",
            "workspace-copying",
            "workspace-ready"
        ],
        "the fallback reports once, each copy tick reports, then ready"
    );
}

#[tokio::test]
async fn a_frozen_copy_reports_its_counts_and_the_clean_clone_fix() {
    let engine = ScriptedEngine {
        readiness: ReadinessScript::StatusFrozen(
            "v=1 phase=copy strategy=copy-in files=9000 bytes=123456",
        ),
        ..ScriptedEngine::default()
    };
    let (result, _) = run_single_job(engine).await;
    let error = result.expect_err("a frozen copy must fail").to_string();
    assert!(error.contains("9000 files"), "{error}");
    assert!(error.contains("clean clone"), "{error}");
    assert!(
        !error.contains("`sh` and `tail`"),
        "the stall never blames the image's shell: {error}"
    );
}

#[tokio::test]
async fn an_init_failure_surfaces_its_own_detail() {
    let engine = ScriptedEngine {
        readiness: ReadinessScript::StatusFrozen(
            "v=1 phase=failed\ncould not copy /greenlit/lower/x: permission denied",
        ),
        ..ScriptedEngine::default()
    };
    let (result, _) = run_single_job(engine).await;
    let error = result
        .expect_err("phase=failed must fail the run")
        .to_string();
    assert!(
        error.contains("could not copy /greenlit/lower/x: permission denied"),
        "init's own message surfaces: {error}"
    );
}

#[tokio::test]
async fn a_container_that_never_reports_names_the_init_helper() {
    let engine = ScriptedEngine {
        readiness: ReadinessScript::StatusFrozen(""),
        ..ScriptedEngine::default()
    };
    let (result, _) = run_single_job(engine).await;
    let error = result.expect_err("silence must time out").to_string();
    assert!(error.contains("never reported isolation status"), "{error}");
    assert!(error.contains("greenlit-init"), "{error}");
}

#[tokio::test]
async fn an_unprobeable_container_gets_the_sh_fix() {
    let engine = ScriptedEngine {
        readiness: ReadinessScript::ProbeExecErrors,
        ..ScriptedEngine::default()
    };
    let (result, _) = run_single_job(engine).await;
    let error = result
        .expect_err("an unprobeable container fails")
        .to_string();
    assert!(
        error.contains("could not probe the job container"),
        "{error}"
    );
    assert!(error.contains("POSIX `sh`"), "{error}");
}
