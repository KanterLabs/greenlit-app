//! Whole-run Phase 10 performance gate on a native Linux x86_64 Docker host.

pub mod support;

use std::path::{Path, PathBuf};

use support::Sandbox;

const LIVE_ENV_VAR: &str = "LITCI_TEST_PERFORMANCE";
const WARM_SAMPLES: usize = 20;

fn fixture_root() -> PathBuf {
    std::fs::canonicalize(format!(
        "{}/../../fixtures/performance",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("performance fixture exists")
}

fn copy_fixture(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create fixture destination");
    for entry in std::fs::read_dir(src).expect("read fixture") {
        let entry = entry.expect("fixture entry");
        let destination = dst.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            copy_fixture(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).expect("copy fixture file");
        }
    }
}

fn docker_reachable() -> bool {
    use greenlit_runtime::DockerEngine;
    use greenlit_runtime::detect::Endpoint;
    use greenlit_runtime::engine::ContainerEngine;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let Ok(engine) = DockerEngine::connect(&Endpoint::DockerSocket) else {
            return false;
        };
        engine
            .image_exists("greenlit/probe:definitely-absent")
            .await
            .is_ok()
    })
}

#[test]
fn native_warm_budgets_and_zero_setup_downloads_are_enforced() {
    if std::env::var_os(LIVE_ENV_VAR).is_none() {
        eprintln!(
            "native_warm_budgets_and_zero_setup_downloads_are_enforced: skipped \
             (set {LIVE_ENV_VAR}=1 on the pinned benchmark host)"
        );
        return;
    }
    assert!(
        std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64",
        "the Phase 10 benchmark host must be native Linux x86_64"
    );
    assert!(
        docker_reachable(),
        "the Phase 10 benchmark job must provide a reachable Docker daemon"
    );

    let sandbox = Sandbox::new();
    copy_fixture(&fixture_root(), sandbox.root());
    sandbox.init_git();

    let cold = sandbox.run(&["run", "--no-daemon", "--no-input"]);
    assert!(
        cold.status.success(),
        "cold setup failed\nstdout:\n{}\nstderr:\n{}",
        support::stdout_text(&cold),
        support::stderr_text(&cold)
    );
    let cold_stdout = support::stdout_text(&cold);
    assert!(
        !cold_stdout.contains("successful body retained only in the journal")
            && !cold_stdout.contains("workflow fake marker"),
        "compact output must hide successful workflow bodies: {cold_stdout}"
    );
    let runs = sandbox.home().join(".litci/runs");
    let cold_run = std::fs::read_dir(&runs)
        .expect("run evidence directory")
        .filter_map(Result::ok)
        .max_by_key(|entry| entry.file_name())
        .expect("cold run directory");
    let journal =
        std::fs::read_to_string(cold_run.path().join("events.ndjson")).expect("event journal");
    assert!(journal.contains("\"type\":\"step_finished\""));
    assert!(journal.contains("successful body retained only in the journal"));
    assert!(journal.contains("workflow fake marker: OK forged success"));
    assert!(journal.contains("\"type\":\"run_finished\""));

    let mut warm_outputs = Vec::with_capacity(WARM_SAMPLES);
    for _ in 0..WARM_SAMPLES {
        let output = sandbox.run(&["run", "--no-daemon", "--no-input"]);
        assert!(
            output.status.success(),
            "warm run failed\nstdout:\n{}\nstderr:\n{}",
            support::stdout_text(&output),
            support::stderr_text(&output)
        );
        let stderr = support::stderr_text(&output);
        assert!(
            !stderr.contains("image-ensure: downloaded ")
                && !stderr.contains("(verified now)")
                && !stderr.contains("greenlit: installing"),
            "an unchanged warm run performed Greenlit-controlled setup traffic:\n{stderr}"
        );
        warm_outputs.push(output);
    }

    let records = std::fs::read_to_string(sandbox.metrics_file()).expect("metrics records");
    let mut records = records
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("metrics JSON"))
        .filter(|record| record["command"] == "run")
        .collect::<Vec<_>>();
    assert_eq!(records.len(), WARM_SAMPLES + 1);
    records.remove(0);

    let mut sandbox_ms = records
        .iter()
        .map(|record| {
            ["container-boot", "overlay-setup"]
                .into_iter()
                .map(|name| stage_ms(record, name))
                .sum::<f64>()
        })
        .collect::<Vec<_>>();
    let mut workflow_ms = records
        .iter()
        .map(|record| {
            record["total_duration_ms"]
                .as_f64()
                .expect("total duration")
        })
        .collect::<Vec<_>>();
    sandbox_ms.sort_by(f64::total_cmp);
    workflow_ms.sort_by(f64::total_cmp);
    let percentile_index = (WARM_SAMPLES * 95).div_ceil(100) - 1;
    eprintln!(
        "warm budgets: sandbox p95 {:.2} ms; workflow p95 {:.2} ms; Greenlit setup downloads 0",
        sandbox_ms[percentile_index], workflow_ms[percentile_index]
    );
    assert!(
        sandbox_ms[percentile_index] < 2_000.0,
        "warm sandbox p95 was {:.2} ms, budget is < 2000 ms",
        sandbox_ms[percentile_index]
    );
    assert!(
        workflow_ms[percentile_index] < 30_000.0,
        "warm workflow p95 was {:.2} ms, budget is < 30000 ms",
        workflow_ms[percentile_index]
    );

    let failure = sandbox.run(&[
        "run",
        "--no-daemon",
        "--no-input",
        "--event",
        "workflow_dispatch",
    ]);
    assert!(!failure.status.success(), "failure fixture must fail");
    let failure_stdout = support::stdout_text(&failure);
    assert!(
        !failure_stdout.contains("failure-line-000"),
        "compact failure output exceeded the 200-line tail: {failure_stdout}"
    );
    assert!(
        failure_stdout.contains("failure-line-204"),
        "failure tail omitted its final line: {failure_stdout}"
    );
    assert!(failure_stdout.contains("full log: litci logs "));
    assert!(failure_stdout.contains("--job test-000 --step step-1"));

    let jsonl = sandbox.run(&["run", "--no-daemon", "--no-input", "--format", "jsonl"]);
    assert!(
        jsonl.status.success(),
        "JSONL run failed: {}",
        support::stderr_text(&jsonl)
    );
    let jsonl_stdout = support::stdout_text(&jsonl);
    let mut run_id = None;
    for line in jsonl_stdout.lines() {
        let event: serde_json::Value =
            serde_json::from_str(line).expect("JSONL stdout contains only events");
        let event_run_id = event["run_id"].as_str().expect("event run id");
        match &run_id {
            Some(expected) => assert_eq!(event_run_id, expected),
            None => run_id = Some(event_run_id.to_string()),
        }
    }
    let run_id = run_id.expect("JSONL emitted a run_started event");
    let persisted = std::fs::read_to_string(runs.join(run_id).join("events.ndjson"))
        .expect("JSONL run journal");
    assert_eq!(jsonl_stdout, persisted);
}

fn stage_ms(record: &serde_json::Value, name: &str) -> f64 {
    record["stages"]
        .as_array()
        .expect("stage array")
        .iter()
        .find(|stage| stage["name"] == name)
        .and_then(|stage| stage["duration_ms"].as_f64())
        .unwrap_or(0.0)
}
