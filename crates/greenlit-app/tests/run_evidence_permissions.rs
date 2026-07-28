//! Compiled-binary coverage for private run-evidence creation and rejection.

pub mod support;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use support::Sandbox;

const WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo private evidence
";

fn run_with_umask(sandbox: &Sandbox, umask: &str) -> Output {
    let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    Command::new("sh")
        .arg("-c")
        .arg(format!("umask {umask}; exec \"$@\""))
        .arg("litci-permission-test")
        .arg(env!("CARGO_BIN_EXE_litci"))
        .args(["run", "--no-daemon", "--no-input", "--allow-degraded"])
        .current_dir(sandbox.root())
        .env_clear()
        .env("PATH", path)
        .env("HOME", sandbox.home())
        .env("XDG_CONFIG_HOME", sandbox.home().join(".config"))
        .env("LITCI_TEST_NO_KEYRING", "1")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("DOCKER_HOST", "ssh://example")
        .output()
        .expect("spawn litci under the selected umask")
}

fn assert_mode(path: &Path, expected: u32) {
    let metadata = std::fs::symlink_metadata(path).expect("inspect retained evidence path");
    assert!(
        !metadata.file_type().is_symlink(),
        "{} must not be a symlink",
        path.display()
    );
    assert_eq!(
        metadata.permissions().mode() & 0o7777,
        expected,
        "{} has the wrong mode",
        path.display()
    );
}

fn run_directories(runs: &Path) -> Vec<std::path::PathBuf> {
    let mut directories = std::fs::read_dir(runs)
        .expect("read run evidence root")
        .map(|entry| entry.expect("read run evidence entry").path())
        .collect::<Vec<_>>();
    directories.sort();
    directories
}

fn manifest_mode(manifest: &[serde_json::Value], path: &str) -> u64 {
    manifest
        .iter()
        .find(|entry| entry["path"] == path)
        .and_then(|entry| entry["mode"].as_u64())
        .unwrap_or_else(|| panic!("source manifest is missing {path}"))
}

fn assert_unsafe_mode_diagnostic(stderr: &str, path: &str, actual_mode: &str) {
    assert!(
        stderr.to_ascii_lowercase().contains("unsafe")
            && stderr.contains(path)
            && stderr.contains(actual_mode)
            && stderr.contains("700"),
        "unsafe path needs one actionable diagnostic: {stderr}"
    );
}

fn assert_private_tree(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).expect("inspect retained tree entry");
        if metadata.file_type().is_symlink() {
            continue;
        }
        let mode = metadata.permissions().mode() & 0o7777;
        if metadata.is_dir() {
            assert_eq!(mode, 0o700, "{} is not a private directory", path.display());
            pending.extend(
                std::fs::read_dir(&path)
                    .expect("read retained directory")
                    .map(|entry| entry.expect("read retained entry").path()),
            );
        } else {
            let expected = if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("greenlit-init-"))
                && path
                    .parent()
                    .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "runtime"))
            {
                0o700
            } else {
                0o600
            };
            assert_eq!(
                mode,
                expected,
                "{} is not a private Greenlit file",
                path.display()
            );
        }
    }
}

#[test]
fn run_evidence_is_born_private_and_unsafe_parents_are_not_repaired() {
    let sandbox = Sandbox::new();
    std::fs::set_permissions(sandbox.home(), std::fs::Permissions::from_mode(0o2700))
        .expect("make the isolated HOME inherit SGID");
    assert_mode(sandbox.home(), 0o2700);
    sandbox.write(".github/workflows/ci.yml", WORKFLOW);
    let executable = sandbox.write("scripts/check.sh", "#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("make the fixture executable");
    sandbox.init_git();
    sandbox.git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/greenlit/private-source.git",
    ]);

    let first = run_with_umask(&sandbox, "000");
    assert!(
        !first.status.success(),
        "the deliberately unreachable container endpoint unexpectedly succeeded"
    );

    let litci = sandbox.home().join(".litci");
    let runs = litci.join("runs");
    let directories = run_directories(&runs);
    assert_eq!(
        directories.len(),
        1,
        "one failed invocation records one run"
    );
    let run = &directories[0];

    assert_mode(&litci, 0o700);
    assert_mode(&runs, 0o700);
    assert_mode(run, 0o700);
    // Atomic rename preserves the temporary inode's mode. Checking the
    // published JSON files under umask 000 therefore pins their pre-rename
    // creation mode at this compiled-binary boundary.
    for name in [
        "source-manifest.json",
        "trace.ndjson",
        "events.ndjson",
        "result.json",
    ] {
        assert_mode(&run.join(name), 0o600);
    }
    let source = run.join("source");
    assert_private_tree(&source);
    assert_mode(&source.join(".git/config"), 0o600);
    assert_mode(&source.join(".github/workflows/ci.yml"), 0o600);
    assert_mode(&source.join("scripts/check.sh"), 0o600);
    let manifest: Vec<serde_json::Value> = serde_json::from_slice(
        &std::fs::read(run.join("source-manifest.json")).expect("read source manifest"),
    )
    .expect("parse source manifest");
    assert_eq!(
        manifest_mode(&manifest, ".github/workflows/ci.yml"),
        0o100644
    );
    assert_eq!(manifest_mode(&manifest, "scripts/check.sh"), 0o100755);
    assert!(
        std::fs::read_dir(run)
            .expect("read retained run")
            .all(|entry| !entry
                .expect("read retained artifact")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")),
        "atomic publication must not leave a temporary artifact behind"
    );
    assert_private_tree(&litci);

    for umask in ["0077", "0777"] {
        let matrix_sandbox = Sandbox::new();
        std::fs::set_permissions(
            matrix_sandbox.home(),
            std::fs::Permissions::from_mode(0o2700),
        )
        .expect("make the matrix HOME inherit SGID");
        matrix_sandbox.write(".github/workflows/ci.yml", WORKFLOW);
        let executable = matrix_sandbox.write("scripts/check.sh", "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("make the matrix fixture executable");
        matrix_sandbox.init_git();
        matrix_sandbox.git(&[
            "remote",
            "add",
            "origin",
            "https://example.invalid/greenlit/private-source.git",
        ]);

        let output = run_with_umask(&matrix_sandbox, umask);
        assert!(
            !output.status.success(),
            "the deliberately unreachable container endpoint unexpectedly succeeded under umask {umask}"
        );
        let matrix_litci = matrix_sandbox.home().join(".litci");
        assert_private_tree(&matrix_litci);
        assert_eq!(
            run_directories(&matrix_litci.join("runs")).len(),
            1,
            "umask {umask} did not retain exactly one private run"
        );
    }

    std::fs::set_permissions(&litci, std::fs::Permissions::from_mode(0o755))
        .expect("make the state parent unsafe");
    let unsafe_state = run_with_umask(&sandbox, "000");
    assert!(!unsafe_state.status.success());
    let unsafe_state_stderr = support::stderr_text(&unsafe_state);
    assert_unsafe_mode_diagnostic(&unsafe_state_stderr, ".litci", "755");
    assert_mode(&litci, 0o755);
    assert_eq!(
        run_directories(&runs).len(),
        1,
        "rejection must happen before another run directory is created"
    );

    std::fs::set_permissions(&litci, std::fs::Permissions::from_mode(0o700))
        .expect("restore the private state parent");
    std::fs::set_permissions(&runs, std::fs::Permissions::from_mode(0o755))
        .expect("make the runs parent unsafe");
    let unsafe_runs = run_with_umask(&sandbox, "000");
    assert!(!unsafe_runs.status.success());
    let unsafe_runs_stderr = support::stderr_text(&unsafe_runs);
    assert_unsafe_mode_diagnostic(&unsafe_runs_stderr, ".litci/runs", "755");
    assert_mode(&runs, 0o755);
    assert_eq!(
        run_directories(&runs).len(),
        1,
        "rejection must not create or repair a run directory"
    );

    std::fs::set_permissions(&runs, std::fs::Permissions::from_mode(0o700))
        .expect("restore the private runs parent");
    std::fs::set_permissions(&litci, std::fs::Permissions::from_mode(0o2700))
        .expect("add SGID to the existing state parent");
    let unsafe_sgid_state = run_with_umask(&sandbox, "000");
    assert!(!unsafe_sgid_state.status.success());
    let unsafe_sgid_stderr = support::stderr_text(&unsafe_sgid_state);
    assert_unsafe_mode_diagnostic(&unsafe_sgid_stderr, ".litci", "2700");
    assert_mode(&litci, 0o2700);
    assert_eq!(
        run_directories(&runs).len(),
        1,
        "rejection must not repair an existing SGID state directory"
    );
}
