//! Compiled-binary invariant coverage for complete retained-run secret scans.

pub mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::Path;
use std::process::Output;
use std::thread;
use std::time::Duration;

use harness::{
    ContainerGuard, NetworkGuard, RunningLitci, assert_real_docker, docker,
    observe_running_container, one_run_directory, read_container_text, run_directories,
    wait_for_container_path,
};
use support::Sandbox;

#[path = "retained_secret_invariant/harness.rs"]
mod harness;

const RELEASE_MARKER: &str = "/tmp/greenlit-secret-invariant-release";
const EMITTED_MARKER: &str = "/tmp/greenlit-secret-invariant-emitted";
const FINISH_MARKER: &str = "/tmp/greenlit-secret-invariant-finish";
const STORAGE_ENV_CHECKED_MARKER: &str = "/tmp/greenlit-phase12-storage-env-absent";
const DYNAMIC_VALUE_MARKER: &str = "/tmp/greenlit-dynamic-mask-value";
const CLI_SECRET_NAME: &str = "RETAINED_TREE_SECRET";
const ORIGIN_SECRET_VALUE: &str = "ghp_REMOTE_ORIGIN+SENTINEL";
const HELPER_READ_LIMIT_KIB: u64 = 768 * 1024;
const OVERSIZED_HELPER_BYTES: u64 = 1536 * 1024 * 1024;
const SPARSE_HELPER_MAX_ALLOCATED_BYTES: u64 = 1024 * 1024;
const REJECTED_ENGINE_BOUNDARY: (&str, &str) = (
    "DOCKER_HOST",
    "ssh://greenlit-origin-invariant-never-connects",
);

const LEAK_SCRIPT: &str = r#"
if env | grep -Eq '^(ACTIONS_CACHE_URL|ACTIONS_RESULTS_URL|ACTIONS_RUNTIME_TOKEN)='; then
  echo "Phase 12 exposed quarantined ACTIONS_* workflow-storage credentials" >&2
  exit 91
fi
dynamic_prefix=$(printf '\147\150\160\137')
dynamic_mask="${dynamic_prefix}DYNAMIC_MASK_$$_f91c"
printf '%s' "$dynamic_mask" > /tmp/greenlit-dynamic-mask-value
printf '::add-mask::%s\n' "$dynamic_mask"
touch /tmp/greenlit-phase12-storage-env-absent
while [ ! -f /tmp/greenlit-secret-invariant-release ]; do sleep 0.05; done
printf 'direct=%s\n' "$dynamic_mask"
encoded=$(printf '%s' "$dynamic_mask" | base64 | tr -d '\n')
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
percent_encode "$dynamic_mask"
printf '\n'
first=$(printf '%s' "$dynamic_mask" | cut -c 1-17)
rest=$(printf '%s' "$dynamic_mask" | cut -c 18-)
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
    assert_explicit_values_are_rejected_before_retained_state();
    assert_interrupted_source_stage_never_enters_litci_state();
    assert_metrics_failure_after_execution_cannot_turn_green();
    assert_terminal_sync_failure_cannot_leave_mixed_authority();
    assert_result_directory_sync_failure_removes_visible_authority();
    assert_composite_publication_interruption_windows();
    assert_post_catalog_render_failure_preserves_authority();
    assert_restrictive_umask_full_execution_is_private();
    for terminal in [
        TerminalPath::Success,
        TerminalPath::FailedStep,
        TerminalPath::Cancelled,
        TerminalPath::PreparationFailed,
        TerminalPath::ClosedOutput,
    ] {
        exercise_terminal_path(terminal);
    }
}

fn assert_explicit_values_are_rejected_before_retained_state() {
    let cases = [
        ("--secret", format!("{CLI_SECRET_NAME}=abc"), "abc"),
        ("--input", "name=run".to_string(), "run"),
        ("--var", "name=shell".to_string(), "shell"),
    ];
    for (option, assignment, sensitive) in cases {
        let sandbox = Sandbox::new();
        let output = sandbox.run(&["run", "--allow-degraded", option, assignment.as_str()]);
        assert!(
            !output.status.success(),
            "{option} passed explicit credential-bearing preflight"
        );
        let mut representations = sensitive_representations(sensitive);
        if sensitive == "abc" {
            representations.push(b"YWJj".to_vec());
        }
        assert_rendered_bytes_are_clean(&output, &representations);
        assert!(
            !sandbox.home().join(".litci").exists(),
            "{option} created retained state before its fixed rejection"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("blocked before daemon, credential, network, action, or container")
                && stderr.contains("fix:"),
            "{option} rejection was not the fixed actionable boundary: {stderr}"
        );
    }
}

fn assert_interrupted_source_stage_never_enters_litci_state() {
    const SOURCE_SENTINEL: &str = "interrupted-source-sentinel-5f91";
    let sandbox = Sandbox::new();
    let mut source = vec![b'x'; 8 * 1024 * 1024];
    source.extend_from_slice(SOURCE_SENTINEL.as_bytes());
    let source_identity = greenlit_store::cas::ObjectDigest::of_bytes(&source);
    let source_len = source.len() as u64;
    sandbox.write_bytes("large-source.bin", &source);
    let running = RunningLitci::spawn_with_env(
        &sandbox,
        "on: push\njobs:\n  interrupted:\n    runs-on: ubuntu-latest\n    steps:\n      - run: exit 0\n",
        &[("LITCI_TEST_SOURCE_STAGE_HOLD", "after-capture")],
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let stage = loop {
        if let Some(path) = fs::read_dir(sandbox.home())
            .expect("read staging home")
            .map(|entry| entry.expect("read staging entry").path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".greenlit-source-stage-") && !name.contains(".capture-")
                }) && fs::metadata(path.join("large-source.bin"))
                    .is_ok_and(|metadata| metadata.len() == source_len)
            })
        {
            break path;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "private source stage did not appear before interruption deadline"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert_private_tree_modes(&stage);
    running.signal_kill();
    let output = running.finish();
    assert!(
        !output.status.success(),
        "SIGKILL interruption unexpectedly succeeded"
    );
    let representations = sensitive_representations(SOURCE_SENTINEL);
    let litci = sandbox.home().join(".litci");
    assert!(
        litci.is_dir(),
        "interrupted run never allocated private state"
    );
    assert_complete_tree_is_clean(&litci, &representations);
    assert_identities_not_in_shared_cas(&sandbox, &[source_identity]);
    assert_phase_24_storage_absent(&sandbox);
    assert!(
        stage.is_dir(),
        "named private source stage did not preserve the interrupted capture boundary"
    );
    fs::remove_dir_all(&stage).expect("remove interrupted private source stage");
}

fn assert_metrics_failure_after_execution_cannot_turn_green() {
    let sandbox = Sandbox::new();
    sandbox.write_home(".litci/metrics", "force metrics open failure");
    fs::set_permissions(
        sandbox.home().join(".litci"),
        fs::Permissions::from_mode(0o700),
    )
    .expect("make metrics-failure state root private");
    fs::set_permissions(
        sandbox.home().join(".litci/metrics"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("make metrics-failure tripwire private");
    let output = RunningLitci::spawn(
        &sandbox,
        "on: push\njobs:\n  metrics:\n    runs-on: ubuntu-latest\n    steps:\n      - name: execution reached metrics\n        run: exit 0\n",
    )
    .finish();
    assert!(
        !output.status.success(),
        "metrics persistence failure published a successful command"
    );
    let run = one_run_directory(&sandbox);
    assert_execution_succeeded_before_writer_failure(&run);
    assert_unpublished_run_is_aborted(&sandbox, &run, "metrics persistence failure");
    assert_source_not_in_shared_cas(&sandbox, &run);
    assert_phase_24_storage_absent(&sandbox);
    let trace = fs::read_to_string(run.join("trace.ndjson")).expect("read failed metrics trace");
    assert!(
        !trace.contains("\"event\":\"run_completed\""),
        "metrics failure persisted run_completed: {trace}"
    );
    assert_no_rendered_passed(&output, "metrics persistence failure");
}

fn assert_result_directory_sync_failure_removes_visible_authority() {
    let sandbox = Sandbox::new();
    let mut running = RunningLitci::spawn_with_env(
        &sandbox,
        &workflow(TerminalPath::Success),
        &[("LITCI_TEST_RESULT_DIRECTORY_SYNC_FAILURE", "after-rename")],
    );
    let (_run_id, container) = observe_running_container(&sandbox, &mut running);
    let container_cleanup = ContainerGuard::new(container.clone());
    wait_for_container_path(&container, STORAGE_ENV_CHECKED_MARKER);
    let representations = observed_dynamic_representations(&container);
    let source_identities = source_identities(&one_run_directory(&sandbox));
    assert!(
        docker(["exec", &container, "touch", RELEASE_MARKER])
            .status
            .success(),
        "could not release the result directory-sync fault workflow"
    );
    let output = running.finish();
    assert!(
        !output.status.success(),
        "post-rename result directory-sync failure passed"
    );
    assert!(
        run_directories(&sandbox).is_empty(),
        "post-rename sync failure left result.json or its run tree visible"
    );
    assert_no_rendered_passed(&output, "post-rename result sync failure");
    assert_rendered_bytes_are_clean(&output, &representations);
    assert_complete_tree_is_clean(&sandbox.home().join(".litci"), &representations);
    assert_identities_not_in_shared_cas(&sandbox, &source_identities);
    assert_phase_24_storage_absent(&sandbox);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("injected directory sync failure after result rename"),
        "result transaction did not reach the post-rename fault boundary: {stderr}"
    );
    container_cleanup.cleanup();
}

fn assert_terminal_sync_failure_cannot_leave_mixed_authority() {
    let sandbox = Sandbox::new();
    let mut running = RunningLitci::spawn_with_env(
        &sandbox,
        &workflow(TerminalPath::Success),
        &[("LITCI_TEST_TERMINAL_SYNC_FAILURE", "after-write")],
    );
    let (_run_id, container) = observe_running_container(&sandbox, &mut running);
    let container_cleanup = ContainerGuard::new(container.clone());
    wait_for_container_path(&container, STORAGE_ENV_CHECKED_MARKER);
    let representations = observed_dynamic_representations(&container);
    let source_identities = source_identities(&one_run_directory(&sandbox));
    assert!(
        docker(["exec", &container, "touch", RELEASE_MARKER])
            .status
            .success(),
        "could not release the terminal-sync fault workflow"
    );
    let output = running.finish();
    assert!(
        !output.status.success(),
        "terminal journal sync fault published success"
    );
    assert!(
        run_directories(&sandbox).is_empty(),
        "terminal sync fault retained a mixed Passed/Aborted journal"
    );
    assert_no_rendered_passed(&output, "terminal journal sync failure");
    assert_rendered_bytes_are_clean(&output, &representations);
    assert_complete_tree_is_clean(&sandbox.home().join(".litci"), &representations);
    assert_identities_not_in_shared_cas(&sandbox, &source_identities);
    assert_phase_24_storage_absent(&sandbox);
    container_cleanup.cleanup();
}

fn assert_composite_publication_interruption_windows() {
    exercise_publication_interruption(
        "after-terminal-sync",
        "LITCI_TEST_TERMINAL_PUBLICATION_HOLD",
        |sandbox, run, run_id| {
            run.join("events.ndjson").is_file()
                && journal_records(run)
                    .iter()
                    .any(|record| record["type"] == "run_finished")
                && !run.join("result.json").exists()
                && !catalog_has_terminal_state(sandbox, run_id)
        },
    );
    exercise_publication_interruption(
        "after-directory-sync",
        "LITCI_TEST_RESULT_PUBLICATION_HOLD",
        |sandbox, run, run_id| {
            run.join("result.json").is_file() && !catalog_has_terminal_state(sandbox, run_id)
        },
    );
    exercise_publication_interruption(
        "after-catalog-complete",
        "LITCI_TEST_TERMINAL_PUBLICATION_HOLD",
        |sandbox, run, run_id| {
            run.join("result.json").is_file()
                && catalog_has_terminal_state(sandbox, run_id)
                && journal_records(run)
                    .iter()
                    .any(|record| record["type"] == "run_finished")
        },
    );
}

fn assert_post_catalog_render_failure_preserves_authority() {
    let sandbox = Sandbox::new();
    let mut running = RunningLitci::spawn_with_env(
        &sandbox,
        &workflow(TerminalPath::Success),
        &[("LITCI_TEST_TERMINAL_RENDER_FAILURE", "after-catalog")],
    );
    let (run_id, container) = observe_running_container(&sandbox, &mut running);
    let container_cleanup = ContainerGuard::new(container.clone());
    wait_for_container_path(&container, STORAGE_ENV_CHECKED_MARKER);
    let representations = observed_dynamic_representations(&container);
    let run = one_run_directory(&sandbox);
    let source_identities = source_identities(&run);
    assert!(
        docker(["exec", &container, "touch", RELEASE_MARKER])
            .status
            .success(),
        "could not release the post-catalog render-failure workflow"
    );
    let output = running.finish();
    assert!(
        !output.status.success(),
        "post-catalog render failure returned command success"
    );
    assert!(
        run.join("result.json").is_file()
            && catalog_has_terminal_state(&sandbox, &run_id)
            && journal_records(&run)
                .iter()
                .any(|record| record["type"] == "run_finished"),
        "post-catalog presentation failure revoked authoritative evidence"
    );
    assert_result_and_journal_truth(&run, TerminalPath::Success);
    let trace = fs::read_to_string(run.join("trace.ndjson")).expect("read completed run trace");
    assert!(
        trace.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|record| record["event"] == "run_completed")
        }),
        "post-catalog presentation failure lost the completed trace"
    );
    assert_no_rendered_passed(&output, "post-catalog presentation failure");
    assert_rendered_bytes_are_clean(&output, &representations);
    assert_complete_tree_is_clean(&sandbox.home().join(".litci"), &representations);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not render the committed terminal run event")
            && stderr.contains("injected post-catalog render failure"),
        "post-catalog presentation diagnostic was not distinct from workflow failure: {stderr}"
    );
    assert_identities_not_in_shared_cas(&sandbox, &source_identities);
    assert_phase_24_storage_absent(&sandbox);
    container_cleanup.cleanup();
    support::assert_run_resources_removed(&run);
}

fn exercise_publication_interruption(
    point: &str,
    variable: &str,
    reached: impl Fn(&Sandbox, &Path, &str) -> bool,
) {
    let sandbox = Sandbox::new();
    let mut running = RunningLitci::spawn_with_env(
        &sandbox,
        &workflow(TerminalPath::Success),
        &[(variable, point)],
    );
    let (run_id, container) = observe_running_container(&sandbox, &mut running);
    let container_cleanup = ContainerGuard::new(container.clone());
    let run = one_run_directory(&sandbox);
    wait_for_container_path(&container, STORAGE_ENV_CHECKED_MARKER);
    let representations = observed_dynamic_representations(&container);
    let source_identities = source_identities(&run);
    assert!(
        docker(["exec", &container, "touch", RELEASE_MARKER])
            .status
            .success(),
        "could not release the {point} interruption workflow"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !reached(&sandbox, &run, &run_id) {
        assert!(
            std::time::Instant::now() < deadline,
            "publication did not reach {point} before the interruption deadline"
        );
        thread::sleep(Duration::from_millis(5));
    }
    running.signal_kill();
    let output = running.finish();
    assert!(
        !output.status.success(),
        "{point} SIGKILL unexpectedly returned success"
    );
    assert_no_rendered_passed(&output, point);
    assert_rendered_bytes_are_clean(&output, &representations);
    assert_complete_tree_is_clean(&sandbox.home().join(".litci"), &representations);
    assert_identities_not_in_shared_cas(&sandbox, &source_identities);
    assert_phase_24_storage_absent(&sandbox);
    match point {
        "after-terminal-sync" => {
            assert!(
                !run.join("result.json").exists() && !catalog_has_terminal_state(&sandbox, &run_id),
                "pre-result interruption became composite authority"
            );
        }
        "after-directory-sync" => {
            assert!(
                run.join("result.json").is_file() && !catalog_has_terminal_state(&sandbox, &run_id),
                "pre-catalog interruption became composite authority"
            );
        }
        "after-catalog-complete" => {
            assert!(
                run.join("result.json").is_file() && catalog_has_terminal_state(&sandbox, &run_id),
                "catalog-complete interruption lost composite authority"
            );
        }
        _ => unreachable!("declared publication interruption point"),
    }
    container_cleanup.cleanup();
    support::assert_run_resources_removed(&run);
}

fn catalog_has_terminal_state(sandbox: &Sandbox, run_id: &str) -> bool {
    greenlit_store::cas::CasStore::open(greenlit_store::cas::CasStore::default_path_under(
        sandbox.home(),
    ))
    .and_then(|store| store.reclaimable_run_ids())
    .is_ok_and(|runs| runs.iter().any(|candidate| candidate == run_id))
}

fn assert_restrictive_umask_full_execution_is_private() {
    let sandbox = Sandbox::new();
    let workflow = "on: push\njobs:\n  private:\n    runs-on: ubuntu-latest\n    steps:\n      - run: exit 0\n";
    let output = RunningLitci::spawn_under_restrictive_umask(&sandbox, workflow).finish();
    assert!(
        output.status.success(),
        "full execution failed under umask 0777: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = sandbox.home().join(".litci");
    let mut pending = vec![state.clone()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).expect("inspect Greenlit state entry");
        if metadata.file_type().is_symlink() {
            continue;
        }
        let mode = metadata.permissions().mode() & 0o7777;
        if metadata.is_dir() {
            assert_eq!(
                mode,
                0o700,
                "{} was not born as a private directory under umask 0777",
                path.display()
            );
            pending.extend(
                fs::read_dir(&path)
                    .expect("walk Greenlit state directory")
                    .map(|entry| entry.expect("read Greenlit state entry").path()),
            );
            continue;
        }
        let expected_mode = if is_runtime_helper(&path) {
            0o700
        } else {
            0o600
        };
        assert_eq!(
            mode,
            expected_mode,
            "{} was not born with its exact private mode under umask 0777",
            path.display()
        );
    }
    let run = one_run_directory(&sandbox);
    assert!(
        run.join("result.json").is_file(),
        "restrictive-umask full execution did not publish a result"
    );
    support::assert_run_resources_removed(&run);
    let first_helpers = runtime_helpers(&state);
    assert_eq!(
        first_helpers.len(),
        1,
        "full execution did not retain exactly one digest-addressed runtime helper"
    );
    let second = RunningLitci::spawn_under_restrictive_umask(&sandbox, workflow).finish();
    assert!(
        second.status.success(),
        "second full execution failed under umask 0777: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_run = run_directories(&sandbox)
        .into_iter()
        .find(|candidate| candidate != &run)
        .expect("second restrictive-umask execution retained its own run identity");
    assert!(
        second_run.join("result.json").is_file(),
        "second restrictive-umask execution did not publish a result"
    );
    support::assert_run_resources_removed(&second_run);
    assert_eq!(
        runtime_helpers(&state),
        first_helpers,
        "the second run did not reuse the same durable runtime helper identity"
    );

    let helper = first_helpers
        .into_iter()
        .next()
        .expect("one reusable runtime helper");
    let existing_runs = run_directories(&sandbox);
    fs::remove_file(&helper).expect("remove valid runtime helper");
    let oversized = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&helper)
        .expect("create sparse runtime helper collision");
    oversized
        .set_len(OVERSIZED_HELPER_BYTES)
        .expect("extend sparse runtime helper collision");
    drop(oversized);
    let sparse_metadata = fs::metadata(&helper).expect("inspect sparse runtime helper collision");
    assert!(
        sparse_metadata.blocks().saturating_mul(512) <= SPARSE_HELPER_MAX_ALLOCATED_BYTES,
        "oversized helper collision consumed physical storage instead of remaining sparse"
    );

    let bounded =
        RunningLitci::spawn_with_address_space_limit(&sandbox, workflow, HELPER_READ_LIMIT_KIB)
            .finish();
    let collision_metadata =
        fs::metadata(&helper).expect("inspect rejected sparse runtime helper collision");
    let retained_helpers = runtime_helpers(&state);
    fs::remove_file(&helper).expect("remove sparse runtime helper collision");
    assert_eq!(
        bounded.status.code(),
        Some(1),
        "oversized helper collision did not fail through the bounded CLI error path"
    );
    let stderr = String::from_utf8_lossy(&bounded.stderr);
    assert!(
        stderr.contains("bytes do not match the embedded helper digest")
            && stderr.contains("remove this file and retry"),
        "oversized helper collision lacked the bounded actionable diagnostic: {stderr}"
    );
    assert_eq!(
        collision_metadata.len(),
        OVERSIZED_HELPER_BYTES,
        "helper staging modified the rejected sparse collision"
    );
    assert_eq!(
        collision_metadata.permissions().mode() & 0o7777,
        0o700,
        "helper staging changed the rejected collision mode"
    );
    assert_eq!(
        retained_helpers,
        [helper],
        "helper staging replaced the rejected collision or left a partial sibling"
    );
    let mut bounded_runs = run_directories(&sandbox)
        .into_iter()
        .filter(|candidate| !existing_runs.contains(candidate))
        .collect::<Vec<_>>();
    assert_eq!(
        bounded_runs.len(),
        1,
        "oversized helper collision did not retain exactly one failed run identity"
    );
    let bounded_run = bounded_runs.pop().expect("one bounded helper run");
    assert_result_and_journal_truth(&bounded_run, TerminalPath::PreparationFailed);
    assert!(
        journal_records(&bounded_run).iter().all(|record| {
            record["type"] != "step_started" && record["type"] != "step_finished"
        }),
        "oversized helper collision started untrusted workflow execution"
    );
    let bounded_run_id = bounded_run
        .file_name()
        .and_then(|name| name.to_str())
        .expect("bounded helper run has a UTF-8 identity");
    let catalog = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(sandbox.home()),
    )
    .expect("open bounded helper run catalog");
    assert_eq!(
        catalog
            .run_state(bounded_run_id)
            .expect("read bounded helper run state"),
        Some(greenlit_store::cas::RunCatalogState::Completed),
        "oversized helper collision did not publish authoritative preparation-failure evidence"
    );
    support::assert_run_resources_removed(&bounded_run);
    assert!(
        runtime_helpers(&state).is_empty(),
        "sparse helper cleanup left a runtime publication behind"
    );
}

fn is_runtime_helper(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(digest) = name.strip_prefix("greenlit-init-") else {
        return false;
    };
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && path
            .parent()
            .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "runtime"))
}

fn runtime_helpers(state: &Path) -> Vec<std::path::PathBuf> {
    let runtime = state.join("runtime");
    let mut helpers = fs::read_dir(&runtime)
        .expect("read durable runtime directory")
        .map(|entry| entry.expect("read durable runtime entry").path())
        .collect::<Vec<_>>();
    helpers.sort();
    assert!(
        helpers.iter().all(|path| is_runtime_helper(path)),
        "runtime directory retained an unexpected entry"
    );
    helpers
}

fn exercise_terminal_path(terminal: TerminalPath) {
    let sandbox = Sandbox::new();
    symlink(
        "../ordinary-source-target",
        sandbox.root().join("ordinary-source-link"),
    )
    .expect("create ordinary source symlink");
    let workflow = workflow(terminal);
    let mut running = RunningLitci::spawn(&sandbox, &workflow);
    let (run_id, container) = observe_running_container(&sandbox, &mut running);
    let container_cleanup = ContainerGuard::new(container.clone());
    wait_for_container_path(&container, STORAGE_ENV_CHECKED_MARKER);
    let representations = observed_dynamic_representations(&container);
    let run = one_run_directory(&sandbox);
    let source_identities = source_identities(&run);
    let network_collision = matches!(terminal, TerminalPath::PreparationFailed)
        .then(|| NetworkGuard::create(format!("greenlit-run-{run_id}-prepare-000")));

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
        running.close_stdout_after_line(b"    split=***\n");
        assert!(
            docker(["exec", &container, "touch", FINISH_MARKER])
                .status
                .success(),
            "could not release the closed-output terminal path"
        );
    }
    let output = running.finish();

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
        assert_unpublished_run_is_aborted(&sandbox, &run, "closed-output failure");
    } else {
        assert!(
            run.join("result.json").is_file(),
            "terminal path did not publish a scanned result"
        );
        assert_result_and_journal_truth(&run, terminal);
        assert_exact_shell_degradation_is_retained(&run);
    }
    assert_rendered_bytes_are_clean(&output, &representations);
    assert_complete_tree_is_clean(&sandbox.home().join(".litci"), &representations);
    assert_identities_not_in_shared_cas(&sandbox, &source_identities);
    assert_phase_24_storage_absent(&sandbox);
    if let Some(network) = network_collision {
        network.cleanup();
    }
    container_cleanup.cleanup();
    support::assert_run_resources_removed(&run);
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
        let output = sandbox.run_with_env(
            &[
                "run",
                "--no-daemon",
                "--no-input",
                "--allow-degraded",
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
                assert_complete_tree_is_clean(&sandbox.home().join(".litci"), &representations);
                assert_source_not_in_shared_cas(&sandbox, &run);
                assert_phase_24_storage_absent(&sandbox);
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
                if sandbox.home().join(".litci").exists() {
                    assert_complete_tree_is_clean(&sandbox.home().join(".litci"), &representations);
                }
            }
        }
    }
}

#[test]
fn credential_bytes_in_a_retained_symlink_target_block_result_publication() {
    assert_real_docker();
    let sandbox = Sandbox::new();
    let mut running = RunningLitci::spawn(&sandbox, &workflow(TerminalPath::Success));
    let (run_id, container) = observe_running_container(&sandbox, &mut running);
    let container_cleanup = ContainerGuard::new(container.clone());
    let run = one_run_directory(&sandbox);
    wait_for_container_path(&container, STORAGE_ENV_CHECKED_MARKER);
    let dynamic_value = read_container_text(&container, DYNAMIC_VALUE_MARKER);
    let representations = sensitive_representations(&dynamic_value);
    let source_identities = source_identities(&run);
    symlink(
        format!("../{dynamic_value}"),
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
    assert!(!stderr.contains(&dynamic_value), "{stderr}");
    assert_rendered_bytes_are_clean(&output, &representations);
    assert_complete_tree_is_clean(&sandbox.home().join(".litci"), &representations);
    assert_identities_not_in_shared_cas(&sandbox, &source_identities);
    assert_phase_24_storage_absent(&sandbox);
    container_cleanup.cleanup();
    support::assert_run_resources_removed(&run);
}

#[test]
fn unsafe_retained_file_mode_blocks_result_and_terminal_publication() {
    assert_real_docker();
    let sandbox = Sandbox::new();
    let mut running = RunningLitci::spawn(&sandbox, &workflow(TerminalPath::Success));
    let (run_id, container) = observe_running_container(&sandbox, &mut running);
    let container_cleanup = ContainerGuard::new(container.clone());
    let run = one_run_directory(&sandbox);
    wait_for_container_path(&container, STORAGE_ENV_CHECKED_MARKER);
    let representations = observed_dynamic_representations(&container);
    let source_identities = source_identities(&run);
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
    assert_rendered_bytes_are_clean(&output, &representations);
    assert_complete_tree_is_clean(&sandbox.home().join(".litci"), &representations);
    assert_identities_not_in_shared_cas(&sandbox, &source_identities);
    assert_phase_24_storage_absent(&sandbox);
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

fn assert_unpublished_run_is_aborted(sandbox: &Sandbox, run: &Path, context: &str) {
    assert!(
        !run.join("result.json").exists(),
        "{context} published result.json"
    );
    let terminals = journal_records(run)
        .into_iter()
        .filter(|record| record["type"] == "run_finished")
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "{context} did not retain exactly one terminal event"
    );
    assert_eq!(
        terminals[0]["conclusion"], "Aborted",
        "{context} retained a non-aborted terminal"
    );
    let run_id = run
        .file_name()
        .and_then(|name| name.to_str())
        .expect("run identity");
    let store = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(sandbox.home()),
    )
    .expect("open retained content catalog");
    assert!(
        store
            .reclaimable_run_ids()
            .expect("read terminal catalog runs")
            .iter()
            .any(|candidate| candidate == run_id),
        "{context} did not mark the catalog run aborted"
    );
}

fn assert_no_rendered_passed(output: &Output, context: &str) {
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

fn assert_private_tree_modes(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).expect("inspect private staged source");
        if metadata.file_type().is_symlink() {
            continue;
        }
        let expected = if metadata.is_dir() { 0o700 } else { 0o600 };
        assert_eq!(
            metadata.permissions().mode() & 0o7777,
            expected,
            "{} was not privately staged",
            path.display()
        );
        if metadata.is_dir() {
            pending.extend(
                fs::read_dir(&path)
                    .expect("read private staged source")
                    .map(|entry| entry.expect("read staged source entry").path()),
            );
        }
    }
}

fn assert_source_not_in_shared_cas(sandbox: &Sandbox, run: &Path) {
    assert_identities_not_in_shared_cas(sandbox, &source_identities(run));
}

fn source_identities(run: &Path) -> Vec<greenlit_store::cas::ObjectDigest> {
    let manifest_bytes = fs::read(run.join("source-manifest.json")).expect("read source manifest");
    let canonical_manifest = manifest_bytes
        .strip_suffix(b"\n")
        .expect("source manifest has one trailing newline");
    let manifest: serde_json::Value =
        serde_json::from_slice(canonical_manifest).expect("parse source manifest");
    let computed_snapshot = greenlit_store::cas::ObjectDigest::of_bytes(canonical_manifest);
    let snapshot = fs::read(run.join("run-lock.json")).map_or(computed_snapshot, |bytes| {
        let lock: serde_json::Value = serde_json::from_slice(&bytes).expect("parse RunLock");
        greenlit_store::cas::ObjectDigest::parse(
            lock["source"]["snapshot_digest"]
                .as_str()
                .expect("source snapshot identity"),
        )
        .expect("valid source snapshot identity")
    });
    std::iter::once(snapshot)
        .chain(
            manifest
                .as_array()
                .expect("source manifest entries")
                .iter()
                .map(|entry| {
                    greenlit_store::cas::ObjectDigest::parse(
                        entry["digest"].as_str().expect("source entry identity"),
                    )
                    .expect("valid source entry identity")
                }),
        )
        .collect()
}

fn assert_identities_not_in_shared_cas(
    sandbox: &Sandbox,
    identities: &[greenlit_store::cas::ObjectDigest],
) {
    let store = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(sandbox.home()),
    )
    .expect("open shared CAS");
    for identity in identities {
        assert!(
            store
                .read_verified(identity)
                .expect("inspect source CAS containment")
                .is_none(),
            "Phase 12 published frozen source identity {identity} into shared CAS"
        );
    }
}

fn assert_phase_24_storage_absent(sandbox: &Sandbox) {
    for relative in ["cache", "artifacts", "toolcache", "package-cache"] {
        let path = sandbox.home().join(".litci").join(relative);
        assert!(
            !path.exists(),
            "Phase 12 created quarantined workflow-storage state at {}",
            path.display()
        );
    }
}

fn observed_dynamic_representations(container: &str) -> Vec<Vec<u8>> {
    let value = read_container_text(container, DYNAMIC_VALUE_MARKER);
    assert!(
        value.starts_with("ghp_DYNAMIC_MASK_"),
        "workflow did not generate the expected credential-shaped dynamic mask"
    );
    sensitive_representations(&value)
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
    format!(
        "on: push\njobs:\n  leak:\n    runs-on: ubuntu-latest\n    steps:\n      - shell: bash\n        run: |\n{}\n          {finish}\n{second_job}",
        indent(LEAK_SCRIPT, 10)
    )
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
    let mut redacted = String::from_utf8_lossy(&rendered).into_owned();
    for representation in representations {
        redacted = redacted.replace(
            String::from_utf8_lossy(representation).as_ref(),
            "<credential>",
        );
    }
    for (index, representation) in representations.iter().enumerate() {
        assert!(
            !rendered
                .windows(representation.len())
                .any(|window| window == representation),
            "rendered output contains credential representation {index}; redacted output:\n{redacted}"
        );
    }
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
