//! Portable negative-path coverage for `litci auth`.
//!
//! Successful credential persistence requires the host's secure credential
//! store and belongs to the job that provisions that capability. These cases
//! stop before persistence: invalid piped input and a denied device flow.

use super::support;
use super::support::Sandbox;
use super::support::fake_github::{Canned, FakeGitHub};
use std::os::unix::fs::symlink;

#[test]
fn auth_pat_rejects_empty_piped_input() {
    let sandbox = Sandbox::new();
    let output = sandbox.run_with_stdin(&["auth", "--pat"], &[], "\n");
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("no token was provided"), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
}

#[test]
fn device_flow_reports_denial_with_a_fix() {
    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    let handle = server.serve(vec![
        Canned::json(
            200,
            "OK",
            r#"{"device_code":"d","user_code":"ABCD-EFGH","verification_uri":"https://github.com/login/device","expires_in":900,"interval":0}"#,
        ),
        Canned::json(200, "OK", r#"{"error":"access_denied"}"#),
    ]);

    let sandbox = Sandbox::new();
    let output = sandbox.run_with_env(
        &["auth"],
        &[("LITCI_TEST_GITHUB_OAUTH_BASE_URL", base_url.as_str())],
    );
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(stderr.contains("denied"), "{stderr}");
    assert!(stderr.contains("litci auth"), "{stderr}");
    handle.join().unwrap();
}

#[test]
fn credential_persistence_fails_closed_without_plaintext_or_unsafe_path_writes() {
    const INPUT_TOKEN: &str = "github_pat_portable_failure_secret_74ac";

    let unavailable = Sandbox::new();
    let output = unavailable.run_with_stdin(&["auth", "--pat"], &[], &format!("{INPUT_TOKEN}\n"));
    assert!(!output.status.success());
    let stdout = support::stdout_text(&output);
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("kernel persistent keyring is unavailable"),
        "missing keyring did not produce its actionable diagnostic"
    );
    assert!(
        !stdout.contains(INPUT_TOKEN) && !stderr.contains(INPUT_TOKEN),
        "pasted credential reached rendered output"
    );
    assert!(
        !unavailable.home().join(".litci/auth.json").exists(),
        "missing keyring support created a plaintext credential file"
    );

    let legacy = Sandbox::new();
    let legacy_path = legacy.write_home(".litci/auth.json", "legacy-file-must-remain");
    let output = legacy.run_with_stdin(&["auth", "--pat"], &[], &format!("{INPUT_TOKEN}\n"));
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("legacy plaintext credential"),
        "legacy credential path was not rejected"
    );
    assert!(
        !support::stdout_text(&output).contains(INPUT_TOKEN) && !stderr.contains(INPUT_TOKEN),
        "pasted credential reached output while rejecting a legacy file"
    );
    assert_eq!(
        std::fs::read_to_string(legacy_path).expect("read preserved legacy marker"),
        "legacy-file-must-remain"
    );

    let unsafe_home = Sandbox::new();
    let outside = tempfile::tempdir().expect("outside state directory");
    let marker = outside.path().join("marker");
    std::fs::write(&marker, "outside-must-remain").expect("write outside marker");
    symlink(outside.path(), unsafe_home.home().join(".litci")).expect("link unsafe state path");
    let output = unsafe_home.run_with_stdin(&["auth", "--pat"], &[], &format!("{INPUT_TOKEN}\n"));
    assert!(!output.status.success());
    let stderr = support::stderr_text(&output);
    assert!(
        stderr.contains("unsafe ~/.litci path"),
        "unsafe credential state path was not rejected"
    );
    assert!(
        !support::stdout_text(&output).contains(INPUT_TOKEN) && !stderr.contains(INPUT_TOKEN),
        "pasted credential reached output while rejecting an unsafe path"
    );
    assert_eq!(
        std::fs::read_to_string(marker).expect("read outside marker"),
        "outside-must-remain"
    );
}
