//! Compiled-binary invariant coverage for complete retained-run secret scans.

pub mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Output;
use std::thread;
use std::time::Duration;

use harness::{
    ContainerGuard, NetworkGuard, RunningLitci, assert_real_docker, docker, observe_runtime_token,
    one_run_directory, run_directories, wait_for_container_path, write_container_path,
};
use support::Sandbox;

#[path = "retained_secret_invariant/harness.rs"]
mod harness;

const RELEASE_MARKER: &str = "/tmp/greenlit-secret-invariant-release";
const EMITTED_MARKER: &str = "/tmp/greenlit-secret-invariant-emitted";
const FINISH_MARKER: &str = "/tmp/greenlit-secret-invariant-finish";
const CLI_SECRET_INPUT: &str = "/tmp/greenlit-secret-invariant-cli";
const CLI_SECRET_NAME: &str = "RETAINED_TREE_SECRET";
const CLI_SECRET_VALUE: &str = "gl-stab-069+/retained-cli-value";
const RETAINED_COLLISION_SECRET_NAME: &str = "RETAINED_COLLISION_SECRET";
const RETAINED_COLLISION_SECRET_VALUE: &str = greenlit_engine::SUPPORT_CERTIFICATION_WITNESS;
const ORIGIN_SECRET_NAME: &str = "UNUSED_ORIGIN_SECRET";
const ORIGIN_SECRET_VALUE: &str = "ghp_REMOTE_ORIGIN+SENTINEL";
const REJECTED_ENGINE_BOUNDARY: (&str, &str) = (
    "DOCKER_HOST",
    "ssh://greenlit-origin-invariant-never-connects",
);

const LEAK_SCRIPT: &str = r#"
test -n "$ACTIONS_RUNTIME_TOKEN"
while [ ! -f /tmp/greenlit-secret-invariant-release ]; do sleep 0.05; done
token=$ACTIONS_RUNTIME_TOKEN
printf 'direct=%s\n' "$token"
encoded=$(printf '%s' "$token" | base64 | tr -d '\n')
printf 'base64=%s\n' "$encoded"
percent_encode() {
  value=$1
  while [ -n "$value" ]; do
    rest=${value#?}
    character=${value%"$rest"}
    case "$character" in
      [a-zA-Z0-9.~_-]) printf '%s' "$character" ;;
      *) printf '%%%02X' "'$character" ;;
    esac
    value=$rest
  done
}
printf 'percent='
percent_encode "$token"
printf '\n'
first=$(printf '%s' "$token" | cut -c 1-17)
rest=$(printf '%s' "$token" | cut -c 18-)
printf 'split=%s' "$first"
sleep 0.1
printf '%s\n' "$rest"
touch /tmp/greenlit-secret-invariant-emitted
"#;

#[derive(Clone, Copy, Debug)]
enum TerminalPath {
    Success,
    FailedStep,
    Cancelled,
    PreparationFailed,
    ClosedOutput,
}

#[test]
fn internal_runtime_token_never_reaches_any_retained_terminal_tree() {
    assert_real_docker();
    assert_cli_secret_reference_blocks_after_clean_capture();
    assert_capture_failure_scrubs_sensitive_partial_tree();
    assert_capture_failure_preserves_clean_partial_tree();
    for terminal in [
        TerminalPath::Success,
        TerminalPath::FailedStep,
        TerminalPath::Cancelled,
        TerminalPath::PreparationFailed,
        TerminalPath::ClosedOutput,
    ] {
        exercise_terminal_path(terminal);
    }
    assert_terminal_scan_scrubs_sensitive_run_tree();
}

fn exercise_terminal_path(terminal: TerminalPath) {
    let sandbox = Sandbox::new();
    symlink(
        "../ordinary-source-target",
        sandbox.root().join("ordinary-source-link"),
    )
    .expect("create ordinary source symlink");
    let workflow = workflow(terminal);
    let mut running = if matches!(terminal, TerminalPath::PreparationFailed) {
        RunningLitci::spawn_with_secret(&sandbox, &workflow, CLI_SECRET_NAME, CLI_SECRET_VALUE)
    } else {
        RunningLitci::spawn(&sandbox, &workflow)
    };
    let (run_id, container, token) = observe_runtime_token(&sandbox);
    let container_cleanup = ContainerGuard::new(container.clone());
    let network_collision = matches!(terminal, TerminalPath::PreparationFailed)
        .then(|| NetworkGuard::create(format!("greenlit-run-{run_id}-prepare-000")));
    if matches!(terminal, TerminalPath::PreparationFailed) {
        write_container_path(&container, CLI_SECRET_INPUT, CLI_SECRET_VALUE.as_bytes());
    }

    assert!(
        docker(["exec", &container, "touch", RELEASE_MARKER])
            .status
            .success(),
        "could not release the live invariant step"
    );
    if matches!(terminal, TerminalPath::Cancelled) {
        wait_for_container_path(&container, EMITTED_MARKER);
        thread::sleep(Duration::from_millis(200));
        running.signal_interrupt();
    } else if matches!(terminal, TerminalPath::ClosedOutput) {
        wait_for_container_path(&container, EMITTED_MARKER);
        running.close_stdout();
        assert!(
            docker(["exec", &container, "touch", FINISH_MARKER])
                .status
                .success(),
            "could not release the closed-output terminal path"
        );
    }
    let output = running.finish();
    let run = one_run_directory(&sandbox);
    let mut representations = sensitive_representations(&token);
    if matches!(terminal, TerminalPath::PreparationFailed) {
        representations.extend(sensitive_representations(CLI_SECRET_VALUE));
        representations.sort();
        representations.dedup();
    }

    match terminal {
        TerminalPath::Success => assert!(output.status.success(), "success path failed"),
        TerminalPath::FailedStep
        | TerminalPath::Cancelled
        | TerminalPath::PreparationFailed
        | TerminalPath::ClosedOutput => {
            assert!(
                !output.status.success(),
                "{terminal:?} terminal failure path passed"
            )
        }
    }
    if matches!(terminal, TerminalPath::ClosedOutput) {
        let result_path = run.join("result.json");
        let published_conclusion = fs::read(&result_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|result| result["conclusion"].as_str().map(str::to_string));
        assert!(
            !result_path.exists(),
            "closed output published a {published_conclusion:?} result despite terminal writer failure"
        );
        assert_execution_succeeded_before_writer_failure(&run);
    } else {
        assert!(
            run.join("result.json").is_file(),
            "terminal path did not publish a scanned result"
        );
        assert_result_and_journal_truth(&run, terminal);
        assert_exact_shell_degradation_is_retained(&run);
    }
    assert_rendered_bytes_are_clean(&output, &representations);
    assert_complete_tree_is_clean(&run, &representations);
    if let Some(network) = network_collision {
        network.cleanup();
    }
    container_cleanup.cleanup();
    support::assert_run_resources_removed(&run);
}

fn assert_cli_secret_reference_blocks_after_clean_capture() {
    let sandbox = Sandbox::new();
    let workflow = format!(
        "on: push\njobs:\n  blocked:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ${{{{ secrets.{CLI_SECRET_NAME} }}}}\n"
    );
    let output =
        RunningLitci::spawn_with_secret(&sandbox, &workflow, CLI_SECRET_NAME, CLI_SECRET_VALUE)
            .finish();
    assert!(
        !output.status.success(),
        "a referenced CLI secret bypassed stabilization quarantine"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("secret.context")
            && stderr.contains("blocked before daemon, credential, network, action, or container"),
        "referenced CLI secret did not produce the non-forceable quarantine diagnostic: {stderr}"
    );
    let representations = sensitive_representations(CLI_SECRET_VALUE);
    assert_rendered_bytes_are_clean(&output, &representations);
    let run = one_run_directory(&sandbox);
    assert_complete_tree_is_clean(&run, &representations);
    let result: serde_json::Value = serde_json::from_slice(
        &fs::read(run.join("result.json")).expect("read blocked secret result"),
    )
    .expect("parse blocked secret result");
    assert_eq!(result["conclusion"], "blocked");
    assert_eq!(result["compatibility"], "unsupported");
    assert_eq!(result["assurance"], "none");
}

fn assert_capture_failure_scrubs_sensitive_partial_tree() {
    let sandbox = Sandbox::new();
    sandbox.write("captured-sensitive.txt", RETAINED_COLLISION_SECRET_VALUE);
    force_content_store_open_failure(&sandbox);
    let output = RunningLitci::spawn_with_secret(
        &sandbox,
        "on: push\njobs:\n  capture:\n    runs-on: ubuntu-latest\n    steps:\n      - run: exit 0\n",
        RETAINED_COLLISION_SECRET_NAME,
        RETAINED_COLLISION_SECRET_VALUE,
    )
    .finish();
    assert!(
        !output.status.success(),
        "content-store collision did not fail source preparation"
    );
    assert_rendered_bytes_are_clean(
        &output,
        &sensitive_representations(RETAINED_COLLISION_SECRET_VALUE),
    );
    assert!(
        run_directories(&sandbox).is_empty(),
        "failed capture retained a partial tree containing the declared secret"
    );
}

fn assert_capture_failure_preserves_clean_partial_tree() {
    let sandbox = Sandbox::new();
    sandbox.write("ordinary-source.txt", "ordinary failure evidence");
    force_content_store_open_failure(&sandbox);
    let output = RunningLitci::spawn_with_secret(
        &sandbox,
        "on: push\njobs:\n  capture:\n    runs-on: ubuntu-latest\n    steps:\n      - run: exit 0\n",
        CLI_SECRET_NAME,
        CLI_SECRET_VALUE,
    )
    .finish();
    assert!(
        !output.status.success(),
        "content-store collision did not fail source preparation"
    );
    let run = one_run_directory(&sandbox);
    assert_complete_tree_is_clean(&run, &sensitive_representations(CLI_SECRET_VALUE));
}

fn force_content_store_open_failure(sandbox: &Sandbox) {
    sandbox.write_home(".litci/store", "force content-store open failure");
    fs::set_permissions(
        sandbox.home().join(".litci"),
        fs::Permissions::from_mode(0o700),
    )
    .expect("make test state root private");
}

fn assert_terminal_scan_scrubs_sensitive_run_tree() {
    let sandbox = Sandbox::new();
    let running = RunningLitci::spawn_with_secret(
        &sandbox,
        &workflow(TerminalPath::Success),
        RETAINED_COLLISION_SECRET_NAME,
        RETAINED_COLLISION_SECRET_VALUE,
    );
    let (run_id, container, _token) = observe_runtime_token(&sandbox);
    let container_cleanup = ContainerGuard::new(container.clone());
    let run = one_run_directory(&sandbox);
    assert!(
        docker(["exec", &container, "touch", RELEASE_MARKER])
            .status
            .success(),
        "could not release the retained-collision workflow"
    );
    let output = running.finish();
    assert!(
        !output.status.success(),
        "terminal publication retained evidence containing the declared secret"
    );
    assert!(
        !run.exists(),
        "terminal scan refusal left the sensitive run tree retained"
    );
    assert_rendered_bytes_are_clean(
        &output,
        &sensitive_representations(RETAINED_COLLISION_SECRET_VALUE),
    );
    container_cleanup.cleanup();
    support::assert_run_resources_removed(Path::new(&run_id));
}

fn assert_result_and_journal_truth(run: &Path, terminal: TerminalPath) {
    let (result_conclusion, journal_conclusion) = match terminal {
        TerminalPath::Success => ("passed", "Passed"),
        TerminalPath::FailedStep => ("failed", "Failed"),
        TerminalPath::Cancelled => ("canceled", "Canceled"),
        TerminalPath::PreparationFailed => ("preparation_failed", "PreparationFailed"),
        TerminalPath::ClosedOutput => unreachable!("closed output publishes no result"),
    };
    let result: serde_json::Value =
        serde_json::from_slice(&fs::read(run.join("result.json")).expect("read result"))
            .expect("parse result");
    assert_eq!(result["conclusion"], result_conclusion, "{terminal:?}");
    assert_eq!(result["compatibility"], "degraded", "{terminal:?}");
    assert_eq!(result["assurance"], "none", "{terminal:?}");
    if matches!(terminal, TerminalPath::Cancelled) {
        assert_ne!(result["conclusion"], "passed");
    }

    let terminal_records = journal_records(run)
        .into_iter()
        .filter(|record| record["type"] == "run_finished")
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_records.len(),
        1,
        "{terminal:?} retained more than one terminal event"
    );
    let terminal_record = &terminal_records[0];
    assert_eq!(
        terminal_record["conclusion"], journal_conclusion,
        "{terminal:?}"
    );
    assert_eq!(terminal_record["compatibility"], "Degraded", "{terminal:?}");
    assert_eq!(terminal_record["assurance"], "None", "{terminal:?}");
}

fn assert_exact_shell_degradation_is_retained(run: &Path) {
    let lock: serde_json::Value =
        serde_json::from_slice(&fs::read(run.join("run-lock.json")).expect("read RunLock"))
            .expect("parse RunLock");
    let lock_matches = lock["compatibility"]["findings"]
        .as_array()
        .expect("RunLock support findings")
        .iter()
        .filter(|finding| {
            finding["code"] == "execution.shell"
                && finding["disposition"] == "degraded"
                && finding["scope"] == "jobs.leak.steps[0]"
                && finding["reason"] == "the reachable step executes a shell script"
        })
        .count();
    assert_eq!(lock_matches, 1, "RunLock lost the exact forced finding");

    let journal_matches = journal_records(run)
        .into_iter()
        .filter(|record| {
            record["type"] == "compatibility_finding"
                && record["code"] == "execution.shell"
                && record["disposition"] == "degraded"
                && record["scope"] == "jobs.leak.steps[0]"
                && record["reason"] == "the reachable step executes a shell script"
        })
        .count();
    assert_eq!(
        journal_matches, 1,
        "terminal journal lost the exact forced finding"
    );
}

fn journal_records(run: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(run.join("events.ndjson"))
        .expect("read run event journal")
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse run event"))
        .collect()
}

#[test]
fn remote_origin_credentials_are_contained_at_the_compiled_run_boundary() {
    #[derive(Clone, Copy, Debug)]
    enum ExpectedBoundary {
        CapturedBeforeEngineRejection,
        RejectedBeforeEngine,
    }

    struct OriginCase {
        name: &'static str,
        origin: String,
        expected: ExpectedBoundary,
        origin_representations: Vec<Vec<u8>>,
    }

    let common_percent =
        String::from_utf8(percent_encode(ORIGIN_SECRET_VALUE.as_bytes())).expect("percent UTF-8");
    let fully_percent = ORIGIN_SECRET_VALUE
        .as_bytes()
        .iter()
        .map(|byte| format!("%{byte:02X}"))
        .collect::<String>();
    let bearer_fragment = format!("Bearer%20{common_percent}");
    let cases = [
        OriginCase {
            name: "sanitizable HTTPS userinfo plus query and fragment",
            origin: format!(
                "https://oauth2:{ORIGIN_SECRET_VALUE}@github.com/KanterLabs/greenlit-app.git?access={common_percent}#{bearer_fragment}"
            ),
            expected: ExpectedBoundary::CapturedBeforeEngineRejection,
            origin_representations: vec![
                common_percent.as_bytes().to_vec(),
                bearer_fragment.as_bytes().to_vec(),
            ],
        },
        OriginCase {
            name: "bearer-shaped path",
            origin: format!("https://github.com/KanterLabs/{ORIGIN_SECRET_VALUE}.git"),
            expected: ExpectedBoundary::RejectedBeforeEngine,
            origin_representations: Vec::new(),
        },
        OriginCase {
            name: "percent-encoded bearer-shaped path",
            origin: format!("https://github.com/KanterLabs/{fully_percent}.git"),
            expected: ExpectedBoundary::RejectedBeforeEngine,
            origin_representations: vec![fully_percent.as_bytes().to_vec()],
        },
        OriginCase {
            name: "ambiguous transport with encoded credential",
            origin: format!(
                "credential-helper://github.com/KanterLabs/greenlit-app.git?credential={common_percent}"
            ),
            expected: ExpectedBoundary::RejectedBeforeEngine,
            origin_representations: vec![common_percent.as_bytes().to_vec()],
        },
    ];

    for case in cases {
        let sandbox = Sandbox::new();
        sandbox.write(
            ".github/workflows/retained-secret.yml",
            "on: push\njobs:\n  origin:\n    runs-on: ubuntu-latest\n    steps:\n      - run: exit 0\n",
        );
        sandbox.init_git();
        sandbox.git(&["remote", "add", "origin", &case.origin]);
        let secret_argument = format!("{ORIGIN_SECRET_NAME}={ORIGIN_SECRET_VALUE}");
        let output = sandbox.run_with_env(
            &[
                "run",
                "--no-daemon",
                "--no-input",
                "--allow-degraded",
                "--secret",
                &secret_argument,
                "-W",
                ".github/workflows/retained-secret.yml",
            ],
            &[REJECTED_ENGINE_BOUNDARY],
        );
        assert!(
            !output.status.success(),
            "{} unexpectedly executed through the rejected engine boundary",
            case.name
        );

        let mut representations = sensitive_representations(ORIGIN_SECRET_VALUE);
        representations.extend(case.origin_representations);
        representations.sort();
        representations.dedup();
        assert_rendered_bytes_are_clean(&output, &representations);

        let stderr = String::from_utf8_lossy(&output.stderr);
        match case.expected {
            ExpectedBoundary::CapturedBeforeEngineRejection => {
                assert!(
                    stderr.contains("DOCKER_HOST")
                        && stderr.contains("transport Greenlit's engine backend does not support"),
                    "{} did not progress past source capture to the engine boundary: {stderr}",
                    case.name
                );
                let run = one_run_directory(&sandbox);
                assert!(
                    run.join("source/.git/config").is_file(),
                    "{} did not retain the captured Git configuration",
                    case.name
                );
                assert_complete_tree_is_clean(&run, &representations);
            }
            ExpectedBoundary::RejectedBeforeEngine => {
                assert!(
                    stderr.contains(
                        "remote.origin.url is credential-bearing or uses an ambiguous transport"
                    ) && stderr.contains("fix: remove the credential")
                        && stderr.contains("replace or remove remote.origin.url"),
                    "{} did not render the actionable origin correction: {stderr}",
                    case.name
                );
                assert!(
                    !stderr.contains("DOCKER_HOST") && !stderr.contains("container engine"),
                    "{} reached the engine boundary after unsafe source rejection: {stderr}",
                    case.name
                );
                for run in run_directories(&sandbox) {
                    assert_complete_tree_is_clean(&run, &representations);
                }
            }
        }
    }
}

#[test]
fn credential_bytes_in_a_retained_symlink_target_block_result_publication() {
    assert_real_docker();
    let sandbox = Sandbox::new();
    let running = RunningLitci::spawn(&sandbox, &workflow(TerminalPath::Success));
    let (run_id, container, token) = observe_runtime_token(&sandbox);
    let container_cleanup = ContainerGuard::new(container.clone());
    let run = one_run_directory(&sandbox);
    symlink(
        format!("../ghp_REMOTE_CREDENTIAL_{token}"),
        run.join("credential-target-link"),
    )
    .expect("inject credential-bearing retained symlink");

    assert!(
        docker(["exec", &container, "touch", RELEASE_MARKER])
            .status
            .success(),
        "could not release the live invariant step"
    );
    let output = running.finish();
    assert_scan_rejection_left_no_completion(
        &sandbox,
        &run,
        &run_id,
        &output,
        "credential-bearing retained symlink",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("credential-bearing data"), "{stderr}");
    assert!(!stderr.contains(&token), "{stderr}");
    container_cleanup.cleanup();
    support::assert_run_resources_removed(&run);
}

#[test]
fn unsafe_retained_file_mode_blocks_result_and_terminal_publication() {
    assert_real_docker();
    let sandbox = Sandbox::new();
    let running = RunningLitci::spawn(&sandbox, &workflow(TerminalPath::Success));
    let (run_id, container, _token) = observe_runtime_token(&sandbox);
    let container_cleanup = ContainerGuard::new(container.clone());
    let run = one_run_directory(&sandbox);
    fs::set_permissions(
        run.join("source-manifest.json"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("make retained file mode unsafe");

    assert!(
        docker(["exec", &container, "touch", RELEASE_MARKER])
            .status
            .success(),
        "could not release the live invariant step"
    );
    let output = running.finish();
    assert_scan_rejection_left_no_completion(
        &sandbox,
        &run,
        &run_id,
        &output,
        "unsafe retained file mode",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsafe artifact metadata"), "{stderr}");
    container_cleanup.cleanup();
    support::assert_run_resources_removed(&run);
}

fn assert_scan_rejection_left_no_completion(
    sandbox: &Sandbox,
    run: &Path,
    run_id: &str,
    output: &Output,
    context: &str,
) {
    assert!(
        !output.status.success(),
        "{context} published a successful result"
    );
    assert!(
        !run.join("result.json").exists(),
        "{context} published result.json"
    );
    let removed = !run.exists();
    if !removed {
        let events = fs::read_to_string(run.join("events.ndjson")).expect("read run event journal");
        let passed_terminal = events.lines().any(|line| {
            let record: serde_json::Value = serde_json::from_str(line).expect("parse run event");
            record["type"] == "run_finished" && record["conclusion"] == "Passed"
        });
        assert!(
            !passed_terminal,
            "{context} persisted a Passed terminal: {events}"
        );
        let trace = fs::read_to_string(run.join("trace.ndjson")).expect("read run trace");
        let completed_trace = trace.lines().any(|line| {
            let record: serde_json::Value = serde_json::from_str(line).expect("parse run trace");
            record["event"] == "run_completed"
        });
        assert!(
            !completed_trace,
            "{context} persisted a completed trace: {trace}"
        );
    }
    let store = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(sandbox.home()),
    )
    .expect("open retained content catalog");
    let terminal_catalog_runs = store
        .reclaimable_run_ids()
        .expect("read terminal catalog runs");
    let catalog_is_terminal = terminal_catalog_runs
        .iter()
        .any(|candidate| candidate == run_id);
    assert_eq!(
        catalog_is_terminal, removed,
        "{context} retained-tree removal and aborted catalog state disagree for run {run_id}"
    );
    for (stream, bytes) in [
        ("stdout", output.stdout.as_slice()),
        ("stderr", output.stderr.as_slice()),
    ] {
        let rendered = String::from_utf8_lossy(bytes);
        assert!(
            !rendered.contains("Passed"),
            "{context} rendered a Passed terminal or conclusion on {stream}: {rendered}"
        );
    }
}

fn assert_execution_succeeded_before_writer_failure(run: &Path) {
    let events = fs::read_to_string(run.join("events.ndjson")).expect("read run event journal");
    let mut step_succeeded = false;
    let mut job_succeeded = false;
    for line in events.lines() {
        let record: serde_json::Value = serde_json::from_str(line).expect("parse run event");
        if record["conclusion"] == "success" {
            step_succeeded |= record["type"] == "step_finished";
            job_succeeded |= record["type"] == "job_finished";
        }
    }
    assert!(
        step_succeeded && job_succeeded,
        "closed-output path did not complete execution before its writer failed"
    );
}

fn workflow(terminal: TerminalPath) -> String {
    let finish = match terminal {
        TerminalPath::Success | TerminalPath::PreparationFailed => "exit 0",
        TerminalPath::FailedStep => "exit 17",
        TerminalPath::Cancelled => "while :; do sleep 0.1; done",
        TerminalPath::ClosedOutput => {
            "while [ ! -f /tmp/greenlit-secret-invariant-finish ]; do sleep 0.05; done"
        }
    };
    let second_job = if matches!(terminal, TerminalPath::PreparationFailed) {
        "\n  prepare:\n    needs: leak\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo must-not-start\n"
    } else {
        ""
    };
    let leak_script = if matches!(terminal, TerminalPath::PreparationFailed) {
        format!("{LEAK_SCRIPT}\n{}", cli_secret_leak_script())
    } else {
        LEAK_SCRIPT.to_string()
    };
    format!(
        "on: push\njobs:\n  leak:\n    runs-on: ubuntu-latest\n    steps:\n      - shell: bash\n        run: |\n{}\n          {finish}\n{second_job}",
        indent(&leak_script, 10)
    )
}

fn cli_secret_leak_script() -> String {
    r#"
cli_secret=$(cat /tmp/greenlit-secret-invariant-cli)
printf 'cli-direct=%s\n' "$cli_secret"
encoded=$(printf '%s' "$cli_secret" | base64 | tr -d '\n')
printf 'cli-base64=%s\n' "$encoded"
printf 'cli-percent='
percent_encode "$cli_secret"
printf '\n'
first=$(printf '%s' "$cli_secret" | cut -c 1-13)
rest=$(printf '%s' "$cli_secret" | cut -c 14-)
printf 'cli-split=%s' "$first"
sleep 0.1
printf '%s\n' "$rest"
"#
    .to_string()
}

fn indent(value: &str, spaces: usize) -> String {
    let prefix = " ".repeat(spaces);
    value
        .trim_start_matches('\n')
        .lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_rendered_bytes_are_clean(output: &Output, representations: &[Vec<u8>]) {
    let mut rendered = output.stdout.clone();
    rendered.extend_from_slice(&output.stderr);
    assert_no_sensitive_bytes(&rendered, representations, "rendered output");
}

fn assert_complete_tree_is_clean(root: &Path, representations: &[Vec<u8>]) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        assert_no_sensitive_bytes(
            path.as_os_str().as_encoded_bytes(),
            representations,
            "retained path",
        );
        let metadata = fs::symlink_metadata(&path).expect("inspect retained entry");
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).expect("read retained link target");
            assert_no_sensitive_bytes(
                target.as_os_str().as_encoded_bytes(),
                representations,
                "retained link target",
            );
        } else if metadata.is_dir() {
            pending.extend(
                fs::read_dir(path)
                    .expect("read retained directory")
                    .map(|entry| entry.expect("read retained entry").path()),
            );
        } else {
            assert!(metadata.is_file(), "retained special node is unsafe");
            let bytes = fs::read(&path).expect("read retained artifact");
            assert_no_sensitive_bytes(&bytes, representations, "retained artifact");
        }
    }
}

fn assert_no_sensitive_bytes(bytes: &[u8], representations: &[Vec<u8>], location: &str) {
    for representation in representations {
        assert!(
            !bytes
                .windows(representation.len())
                .any(|window| window == representation),
            "{location} contains credential-bearing bytes"
        );
    }
}

fn sensitive_representations(token: &str) -> Vec<Vec<u8>> {
    let direct = token.as_bytes().to_vec();
    let standard = base64(token.as_bytes(), b'+', b'/', true);
    let url = base64(token.as_bytes(), b'-', b'_', false);
    let percent = percent_encode(token.as_bytes());
    let mut representations = vec![direct, standard, url, percent];
    representations.sort();
    representations.dedup();
    representations
}

fn base64(bytes: &[u8], char62: u8, char63: u8, padded: bool) -> Vec<u8> {
    let mut alphabet = *b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    alphabet[62] = char62;
    alphabet[63] = char63;
    let mut encoded = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or_default();
        let third = chunk.get(2).copied().unwrap_or_default();
        encoded.push(alphabet[usize::from(first >> 2)]);
        encoded.push(alphabet[usize::from(((first & 3) << 4) | (second >> 4))]);
        if chunk.len() > 1 {
            encoded.push(alphabet[usize::from(((second & 15) << 2) | (third >> 6))]);
        } else if padded {
            encoded.push(b'=');
        }
        if chunk.len() > 2 {
            encoded.push(alphabet[usize::from(third & 63)]);
        } else if padded {
            encoded.push(b'=');
        }
    }
    encoded
}

fn percent_encode(bytes: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = Vec::with_capacity(bytes.len());
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte);
        } else {
            encoded.extend_from_slice(&[
                b'%',
                HEX[usize::from(byte >> 4)],
                HEX[usize::from(byte & 15)],
            ]);
        }
    }
    encoded
}
