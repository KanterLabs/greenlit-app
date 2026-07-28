//! Public capability-assessment boundary without substituting the runtime.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use greenlit_engine::{
    CAPABILITY_ACTION_USES, CAPABILITY_CREDENTIAL_GITHUB, CAPABILITY_EVIDENCE_INTEGRITY,
    CAPABILITY_EXECUTION_SHELL, CAPABILITY_INFRASTRUCTURE_DIND, CAPABILITY_REACHABILITY_AMBIGUOUS,
    CAPABILITY_SECRET_CONTEXT, CAPABILITY_SECURITY_BOUNDARY, CAPABILITY_SOURCE_CONTAINMENT,
    CAPABILITY_SOURCE_WRITE_BACK, CapabilityFinding, EventKind, ExecutionPlan, PlanOptions,
    QuarantineOutcome, SyntheticEvent, plan,
};
use greenlit_expr::Value;
use greenlit_runtime::{
    Cancellation, DockerEngine, Endpoint, ExecError, ExecutionEventNull, FlatLogSink,
    IsolationStrategy, ProgressNull, ReadinessConfig, ResourceLimits, RunConfig,
    RuntimeAuthorization, RuntimeCapabilityInputs, RuntimeControl, assess_runtime_capabilities,
    run_plan, run_plan_cancellable, run_plan_with_events_cancellable,
};

#[path = "dockerkit/action_config.rs"]
mod action_config;

fn planned(workflow: &str, deferred_github: &[&str]) -> ExecutionPlan {
    let workflow =
        greenlit_workflow::parse_workflow("quarantine.yml", workflow).expect("parse workflow");
    let event = SyntheticEvent {
        kind: EventKind::Push,
        github: Value::object(vec![(
            "event_name".to_string(),
            Value::String("push".to_string()),
        )]),
        inputs: Value::object(Vec::<(String, Value)>::new()),
        deferred_github_properties: deferred_github
            .iter()
            .map(|property| (*property).to_string())
            .collect::<BTreeSet<_>>(),
    };
    plan(&workflow, &event, &PlanOptions::default()).expect("plan workflow")
}

fn ids(assessment: &greenlit_runtime::RuntimeCapabilityAssessment) -> Vec<&str> {
    assessment
        .decision()
        .blocking_findings()
        .iter()
        .map(|finding| finding.finding().capability_id())
        .collect()
}

#[test]
fn github_and_action_token_shapes_fail_closed() {
    let plan = planned(
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - if: false\n        run: echo skipped\n",
        &[],
    );
    let empty_secrets = Value::object(Vec::<(String, Value)>::new());

    for (github, action_token, expected) in [
        (
            Value::object(vec![(
                "token".to_string(),
                Value::String("credential".to_string()),
            )]),
            None,
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
        (
            Value::object(vec![("token".to_string(), Value::Bool(true))]),
            None,
            CAPABILITY_SECURITY_BOUNDARY,
        ),
        (
            Value::object(Vec::<(String, Value)>::new()),
            Some("credential"),
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
        (
            Value::object(Vec::<(String, Value)>::new()),
            Some(""),
            CAPABILITY_SECURITY_BOUNDARY,
        ),
    ] {
        let inputs =
            RuntimeCapabilityInputs::new(&github, &empty_secrets, action_token, false, false);
        let assessment = assess_runtime_capabilities(&plan, &inputs, &[], true);
        assert_eq!(assessment.decision().outcome(), QuarantineOutcome::Blocked);
        assert_eq!(ids(&assessment), vec![expected]);
    }
}

#[test]
fn explicit_dind_is_nonforceable_without_scanning_shell_text() {
    let plan = planned(
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo docker\n",
        &[],
    );
    let github = Value::object(Vec::<(String, Value)>::new());
    let secrets = Value::object(Vec::<(String, Value)>::new());
    let inputs = RuntimeCapabilityInputs::new(&github, &secrets, None, true, false);
    let assessment = assess_runtime_capabilities(&plan, &inputs, &[], true);
    assert_eq!(assessment.decision().outcome(), QuarantineOutcome::Blocked);
    let ids = ids(&assessment);
    assert!(ids.contains(&CAPABILITY_INFRASTRUCTURE_DIND));
    assert!(!ids.contains(&CAPABILITY_REACHABILITY_AMBIGUOUS));
}

#[test]
fn deferred_run_content_is_shell_content_not_reachability() {
    let plan = planned(
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github.ref }}\n",
        &["ref"],
    );
    let github = Value::object(Vec::<(String, Value)>::new());
    let secrets = Value::object(Vec::<(String, Value)>::new());
    let inputs = RuntimeCapabilityInputs::new(&github, &secrets, None, false, false);
    let assessment = assess_runtime_capabilities(&plan, &inputs, &[], true);
    assert_eq!(assessment.decision().outcome(), QuarantineOutcome::Degraded);
    let forced = assessment
        .decision()
        .forced_findings()
        .iter()
        .map(|finding| finding.finding().capability_id())
        .collect::<Vec<_>>();
    assert_eq!(forced, vec![CAPABILITY_EXECUTION_SHELL]);
    assert!(assessment.decision().blocking_findings().is_empty());
}

#[test]
fn authored_secret_and_github_token_shapes_are_nonforceable_with_empty_runtime_contexts() {
    let cases = [
        (
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ secrets.API_TOKEN }}\n",
            CAPABILITY_SECRET_CONTEXT,
        ),
        (
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ secrets[format('API_{0}', 'TOKEN')] }}\n",
            CAPABILITY_SECRET_CONTEXT,
        ),
        (
            "on: push\njobs:\n  build:\n    strategy:\n      matrix:\n        secret_name: [API_TOKEN]\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ secrets[matrix.secret_name] }}\n",
            CAPABILITY_SECRET_CONTEXT,
        ),
        (
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(secrets.*) }}\n",
            CAPABILITY_SECRET_CONTEXT,
        ),
        (
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(secrets) }}\n",
            CAPABILITY_SECRET_CONTEXT,
        ),
        (
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github.token }}\n",
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
        (
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github[format('to{0}', 'ken')] }}\n",
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
        (
            "on: push\njobs:\n  build:\n    strategy:\n      matrix:\n        token_key: [token]\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github[matrix.token_key] }}\n",
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
        (
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(github.*) }}\n",
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
        (
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(github) }}\n",
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
    ];
    let github = Value::object(Vec::<(String, Value)>::new());
    let secrets = Value::object(Vec::<(String, Value)>::new());

    for (workflow, expected) in cases {
        let plan = planned(workflow, &[]);
        let inputs = RuntimeCapabilityInputs::new(&github, &secrets, None, false, false);
        let assessment = assess_runtime_capabilities(&plan, &inputs, &[], true);
        assert_eq!(
            assessment.decision().outcome(),
            QuarantineOutcome::Blocked,
            "{workflow}"
        );
        assert!(ids(&assessment).contains(&expected), "{workflow}");
    }
}

struct RecordingDockerApi {
    endpoint: Endpoint,
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<usize>>,
}

impl RecordingDockerApi {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Docker API recorder");
        listener
            .set_nonblocking(true)
            .expect("make Docker API recorder nonblocking");
        let address = listener.local_addr().expect("Docker API recorder address");
        let (stop, stopped) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut requests = 0;
            loop {
                if stopped.try_recv().is_ok() {
                    return requests;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .expect("bound Docker API recorder read");
                        let mut request = [0_u8; 4096];
                        if stream.read(&mut request).is_ok_and(|read| read > 0) {
                            requests += 1;
                        }
                        let _ = stream.write_all(
                            b"HTTP/1.1 500 Internal Server Error\r\n\
                              Content-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        );
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("Docker API recording boundary failed: {error}"),
                }
            }
        });
        Self {
            endpoint: Endpoint::DockerHost(format!("tcp://{address}")),
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> usize {
        self.stop.send(()).expect("stop Docker API recorder");
        self.worker
            .take()
            .expect("Docker API recorder worker")
            .join()
            .expect("join Docker API recorder")
    }
}

impl Drop for RecordingDockerApi {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.stop.send(());
            let _ = worker.join();
        }
    }
}

#[tokio::test]
async fn public_run_plan_blocks_protected_findings_before_production_engine_requests() {
    let cases = [
        (
            planned(
                "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n",
                &[],
            ),
            false,
            CAPABILITY_ACTION_USES,
        ),
        (
            planned(
                "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo retained\n",
                &[],
            ),
            true,
            CAPABILITY_SOURCE_WRITE_BACK,
        ),
        (
            planned(
                "on: push\njobs:\n  build:\n    strategy:\n      matrix:\n        secret_name: [API_TOKEN]\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ secrets[matrix.secret_name] }}\n",
                &[],
            ),
            false,
            CAPABILITY_SECRET_CONTEXT,
        ),
        (
            planned(
                "on: push\njobs:\n  build:\n    strategy:\n      matrix:\n        token_key: [token]\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github[matrix.token_key] }}\n",
                &[],
            ),
            false,
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
        (
            planned(
                "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(secrets) }}\n",
                &[],
            ),
            false,
            CAPABILITY_SECRET_CONTEXT,
        ),
        (
            planned(
                "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ toJSON(github) }}\n",
                &[],
            ),
            false,
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
    ];
    for (plan, write_back, expected_capability) in cases {
        let recorder = RecordingDockerApi::start();
        let engine =
            DockerEngine::connect(&recorder.endpoint).expect("construct production Docker engine");
        let repo = tempfile::tempdir().expect("temporary repository");
        let config = RunConfig {
            repo_host_path: repo.path().to_path_buf(),
            workspace: "/workspace".to_string(),
            strategy: IsolationStrategy::default(),
            runner_env: Default::default(),
            github: Value::object(Vec::<(String, Value)>::new()),
            vars: Value::object(Vec::<(String, Value)>::new()),
            inputs: Value::object(Vec::<(String, Value)>::new()),
            secrets: Value::object(Vec::<(String, Value)>::new()),
            initial_masks: Vec::new(),
            volume_namespace: "quarantine-boundary".to_string(),
            locked_images: None,
            write_back,
            dind: false,
            readiness: ReadinessConfig::default(),
            actions: action_config::unreachable_action_config(),
            store: None,
            resources: ResourceLimits::default(),
        };
        let mut output = Vec::new();
        let error = match run_plan(
            &engine,
            &plan,
            &config,
            RuntimeAuthorization::AllowDegradedShell,
            &mut output,
            &mut ProgressNull,
        )
        .await
        {
            Ok(_) => panic!("blocked plan reached runtime execution"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ExecError::CapabilityQuarantined { capability_id, .. }
                if capability_id == expected_capability
        ));
        assert_eq!(recorder.finish(), 0, "blocked plan contacted Docker API");
    }
}

#[tokio::test]
async fn bound_assessment_drift_is_nonforceable_before_production_engine_requests() {
    let plan = planned(
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo assessed\n",
        &[],
    );
    let github = Value::object(Vec::<(String, Value)>::new());
    let secrets = Value::object(Vec::<(String, Value)>::new());
    let inputs = RuntimeCapabilityInputs::new(&github, &secrets, None, false, false);
    let assessment = assess_runtime_capabilities(&plan, &inputs, &[], true);
    assert_eq!(assessment.decision().outcome(), QuarantineOutcome::Degraded);

    let recorder = RecordingDockerApi::start();
    let engine =
        DockerEngine::connect(&recorder.endpoint).expect("construct production Docker engine");
    let repo = tempfile::tempdir().expect("temporary repository");
    let config = RunConfig {
        repo_host_path: repo.path().to_path_buf(),
        workspace: "/workspace".to_string(),
        strategy: IsolationStrategy::default(),
        runner_env: Default::default(),
        github,
        vars: Value::object(Vec::<(String, Value)>::new()),
        inputs: Value::object(Vec::<(String, Value)>::new()),
        secrets,
        initial_masks: Vec::new(),
        volume_namespace: "quarantine-assessment-drift".to_string(),
        locked_images: None,
        write_back: true,
        dind: false,
        readiness: ReadinessConfig::default(),
        actions: action_config::unreachable_action_config(),
        store: None,
        resources: ResourceLimits::default(),
    };
    let cancellation = Cancellation::new();
    let control = RuntimeControl::with_assessment(
        RuntimeAuthorization::AllowDegradedShell,
        &cancellation,
        &assessment,
    );
    let mut output = Vec::new();
    let mut logs = FlatLogSink::new(&mut output);
    let mut events = ExecutionEventNull;
    let error = match run_plan_with_events_cancellable(
        &engine,
        &plan,
        &config,
        control,
        &mut logs,
        &mut events,
        &mut ProgressNull,
    )
    .await
    {
        Ok(_) => panic!("assessment drift reached runtime execution"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ExecError::CapabilityQuarantined {
            capability_id,
            scope,
            ..
        } if capability_id == CAPABILITY_EVIDENCE_INTEGRITY
            && scope == "runtime.capability-assessment"
    ));
    assert_eq!(
        recorder.finish(),
        0,
        "assessment drift contacted Docker API"
    );
}

#[tokio::test]
async fn public_bound_assessment_cannot_force_source_containment_or_unknown_findings() {
    const UNKNOWN_CAPABILITY: &str = "unknown.runtime-boundary";

    let plan = planned(
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo assessed\n",
        &[],
    );
    let github = Value::object(Vec::<(String, Value)>::new());
    let secrets = Value::object(Vec::<(String, Value)>::new());
    let inputs = RuntimeCapabilityInputs::new(&github, &secrets, None, false, false);
    let cases = [
        (
            CapabilityFinding::new(
                CAPABILITY_SOURCE_CONTAINMENT,
                "source.snapshot",
                "source containment is not certified",
            ),
            CAPABILITY_SOURCE_CONTAINMENT,
            "source.snapshot",
        ),
        (
            CapabilityFinding::new(
                UNKNOWN_CAPABILITY,
                "runtime.unknown",
                "an unknown runtime capability is required",
            ),
            UNKNOWN_CAPABILITY,
            "runtime.unknown",
        ),
    ];

    for (finding, expected_capability, expected_scope) in cases {
        let assessment =
            assess_runtime_capabilities(&plan, &inputs, std::slice::from_ref(&finding), true);
        assert_eq!(
            assessment.decision().outcome(),
            QuarantineOutcome::Blocked,
            "{expected_capability}"
        );
        assert_eq!(ids(&assessment), vec![expected_capability]);

        let recorder = RecordingDockerApi::start();
        let engine =
            DockerEngine::connect(&recorder.endpoint).expect("construct production Docker engine");
        let repo = tempfile::tempdir().expect("temporary repository");
        let config = RunConfig {
            repo_host_path: repo.path().to_path_buf(),
            workspace: "/workspace".to_string(),
            strategy: IsolationStrategy::default(),
            runner_env: Default::default(),
            github: github.clone(),
            vars: Value::object(Vec::<(String, Value)>::new()),
            inputs: Value::object(Vec::<(String, Value)>::new()),
            secrets: secrets.clone(),
            initial_masks: Vec::new(),
            volume_namespace: format!("quarantine-bound-{expected_capability}"),
            locked_images: None,
            write_back: false,
            dind: false,
            readiness: ReadinessConfig::default(),
            actions: action_config::unreachable_action_config(),
            store: None,
            resources: ResourceLimits::default(),
        };
        let cancellation = Cancellation::new();
        let control = RuntimeControl::with_assessment(
            RuntimeAuthorization::AllowDegradedShell,
            &cancellation,
            &assessment,
        );
        let mut output = Vec::new();
        let mut logs = FlatLogSink::new(&mut output);
        let mut events = ExecutionEventNull;
        let error = match run_plan_with_events_cancellable(
            &engine,
            &plan,
            &config,
            control,
            &mut logs,
            &mut events,
            &mut ProgressNull,
        )
        .await
        {
            Ok(_) => panic!("{expected_capability} reached runtime execution"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ExecError::CapabilityQuarantined {
                capability_id,
                scope,
                ..
            } if capability_id == expected_capability && scope == expected_scope
        ));
        assert_eq!(
            recorder.finish(),
            0,
            "{expected_capability} contacted Docker API"
        );
    }
}

#[tokio::test]
async fn public_cancellable_entrypoint_independently_blocks_sensitive_plan_contexts() {
    let cases = [
        (
            planned(
                "on: push\njobs:\n  build:\n    strategy:\n      matrix:\n        secret_name: [API_TOKEN]\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ secrets[matrix.secret_name] }}\n",
                &[],
            ),
            CAPABILITY_SECRET_CONTEXT,
        ),
        (
            planned(
                "on: push\njobs:\n  build:\n    strategy:\n      matrix:\n        token_key: [token]\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{ github[matrix.token_key] }}\n",
                &[],
            ),
            CAPABILITY_CREDENTIAL_GITHUB,
        ),
    ];

    for (plan, expected_capability) in cases {
        let recorder = RecordingDockerApi::start();
        let engine =
            DockerEngine::connect(&recorder.endpoint).expect("construct production Docker engine");
        let repo = tempfile::tempdir().expect("temporary repository");
        let config = RunConfig {
            repo_host_path: repo.path().to_path_buf(),
            workspace: "/workspace".to_string(),
            strategy: IsolationStrategy::default(),
            runner_env: Default::default(),
            github: Value::object(Vec::<(String, Value)>::new()),
            vars: Value::object(Vec::<(String, Value)>::new()),
            inputs: Value::object(Vec::<(String, Value)>::new()),
            secrets: Value::object(Vec::<(String, Value)>::new()),
            initial_masks: Vec::new(),
            volume_namespace: "quarantine-cancellable-boundary".to_string(),
            locked_images: None,
            write_back: false,
            dind: false,
            readiness: ReadinessConfig::default(),
            actions: action_config::unreachable_action_config(),
            store: None,
            resources: ResourceLimits::default(),
        };
        let mut output = Vec::new();
        let cancellation = Cancellation::new();
        let error = match run_plan_cancellable(
            &engine,
            &plan,
            &config,
            RuntimeAuthorization::AllowDegradedShell,
            &mut output,
            &mut ProgressNull,
            &cancellation,
        )
        .await
        {
            Ok(_) => panic!("blocked plan reached runtime execution"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ExecError::CapabilityQuarantined { capability_id, .. }
                if capability_id == expected_capability
        ));
        assert_eq!(recorder.finish(), 0, "blocked plan contacted Docker API");
    }
}
