//! Real-daemon acceptance for [`ContainerEngine::run_container`]'s exit-code
//! race fix. Only Docker can exercise the wait-stream timing of a container
//! that exits immediately; the live-runtime target therefore uses the true
//! external boundary and fails if Docker is unavailable.

#[path = "dockerkit/engine.rs"]
mod engine_support;

use greenlit_runtime::engine::{ContainerEngine, ContainerSpec, ExecOutput, SinkNull};
use greenlit_runtime::progress::ProgressNull;

use engine_support::{required_engine, unique_suffix};

#[tokio::test]
async fn run_container_reports_a_fast_containers_own_exit_code() {
    let engine = required_engine("run_container_reports_a_fast_containers_own_exit_code").await;
    // alpine is the small image the crate's other real-daemon fixtures
    // already pull (`actions_docker.rs`, `actions_composite.rs`).
    engine
        .pull_image("alpine:3.19", None, &mut ProgressNull)
        .await
        .expect("pull alpine");

    let name = format!("greenlit-exit-race-{}", unique_suffix());
    let spec = ContainerSpec {
        image: "alpine:3.19".to_string(),
        name: Some(name),
        // Exits almost immediately after `run_container` starts it, which is
        // exactly the timing that can race the wait stream into missing the
        // real status.
        cmd: vec!["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
        ..ContainerSpec::default()
    };
    let id = engine.create_container(&spec).await.expect("create");

    let mut sink = SinkNull;
    let result = engine.run_container(&id, &mut sink).await;
    let _ = engine.remove_container(&id).await;

    // Pins the fix: a short-lived container reports its own exit code, not
    // the wait-stream-miss placeholder of 1.
    assert_eq!(
        result.expect("run_container"),
        ExecOutput { exit_code: 7 },
        "a short-lived container reports its own exit code, not a placeholder"
    );
}
