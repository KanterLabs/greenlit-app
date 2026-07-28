//! Capability-owned compiled-CLI acceptance for production credential
//! persistence. This target is run only by a Linux job that provisions an
//! isolated persistent keyring; missing prerequisites and cleanup failures
//! are hard failures.

#[path = "credential_capability/containment.rs"]
mod containment;
#[path = "credential_capability/keyring_support.rs"]
mod credential_capability_support;
#[path = "credential_capability/sanitization.rs"]
mod sanitization;
pub mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use containment::assert_plan_and_run_quarantine;
use credential_capability_support::{
    PersistentCredential, assert_clean, assert_failure, assert_path_clean, assert_request_path,
    assert_stderr_contains, assert_stdout_contains, assert_success,
};
use support::Sandbox;
use support::fake_github::{Canned, FakeGitHub};

const WORKFLOW: &str = "\
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    if: vars.MODE == 'ci'
    steps:
      - run: echo ok
";

const PAT_TOKEN: &str = "github_pat_capability_pat_7f91";
const GH_TOKEN: &str = "gho_capability_external_gh_8a42";
const GH_FAILURE_STDOUT: &str = "gho_failure_stdout_secret_5c21";
const GH_FAILURE_STDERR: &str = "gho_failure_stderr_secret_6d32";
const GH_INVALID_FIRST: &str = "gho_invalid_first_secret_7e43";
const GH_INVALID_SECOND: &str = "gho_invalid_second_secret_8f54";
const DEVICE_ACCESS: &str = "ghu_device_access_secret_0a65";
const DEVICE_REFRESH: &str = "ghr_device_refresh_secret_1b76";

#[test]
fn pat_and_external_gh_persist_replace_and_fail_closed_without_leaking() {
    let credential = PersistentCredential::new("pat-gh");
    let sandbox = remote_variable_sandbox();
    let oversized = format!("github_pat_{}", "x".repeat(33_000));
    let secrets = [
        PAT_TOKEN,
        GH_TOKEN,
        GH_FAILURE_STDOUT,
        GH_FAILURE_STDERR,
        GH_INVALID_FIRST,
        GH_INVALID_SECOND,
        oversized.as_str(),
    ];

    let output = sandbox.run_with_credential_keyring_stdin(
        &["auth", "--pat"],
        credential.description(),
        &[],
        &format!("{PAT_TOKEN}\n"),
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_success(&output, "PAT authentication");
    assert_stdout_contains(&output, "system keyring", "PAT authentication");
    assert_stdout_contains(&output, "Contents", "PAT permission guidance");
    assert_stdout_contains(&output, "Variables", "PAT permission guidance");
    assert_stdout_contains(
        &output,
        "Credential use remains quarantined until stabilization Phase 16",
        "PAT authentication",
    );
    let pat_payload = credential.capture_payload();
    assert_plan_and_run_quarantine(&sandbox, &credential, &pat_payload, &secrets);

    let output = sandbox.run_with_credential_keyring_stdin(
        &["auth", "--pat"],
        credential.description(),
        &[],
        &format!("{oversized}\n"),
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_failure(&output, "oversized PAT replacement");
    assert_stderr_contains(&output, "keyring limit", "oversized PAT replacement");
    credential.assert_payload_unchanged(&pat_payload);

    let gh_dir = tempfile::tempdir().expect("fake gh directory");
    write_fake_gh(
        gh_dir.path(),
        "printf '%s\\n' \"$FAKE_GH_TOKEN\"\n\
         exit 0\n",
    );
    let gh_path = gh_dir.path().to_string_lossy().into_owned();
    let output = sandbox.run_with_credential_keyring(
        &["auth", "--gh"],
        credential.description(),
        &[("PATH", gh_path.as_str()), ("FAKE_GH_TOKEN", GH_TOKEN)],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_path_clean(gh_dir.path(), &secrets);
    assert_success(&output, "external gh authentication");
    assert_stdout_contains(&output, "broader scopes", "external gh authentication");
    assert_stdout_contains(
        &output,
        "Credential use remains quarantined until stabilization Phase 16",
        "external gh authentication",
    );
    credential.assert_payload_changed(&pat_payload);
    let gh_payload = credential.capture_payload();
    assert_plan_and_run_quarantine(&sandbox, &credential, &gh_payload, &secrets);

    write_fake_gh(
        gh_dir.path(),
        "printf '%s\\n' \"$FAKE_GH_STDOUT\"\n\
         printf '%s\\n' \"$FAKE_GH_STDERR\" >&2\n\
         exit 41\n",
    );
    let output = sandbox.run_with_credential_keyring(
        &["auth", "--gh"],
        credential.description(),
        &[
            ("PATH", gh_path.as_str()),
            ("FAKE_GH_STDOUT", GH_FAILURE_STDOUT),
            ("FAKE_GH_STDERR", GH_FAILURE_STDERR),
        ],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_path_clean(gh_dir.path(), &secrets);
    assert_failure(&output, "failing external gh replacement");
    assert_stderr_contains(&output, "gh auth login", "failing external gh replacement");
    credential.assert_payload_unchanged(&gh_payload);

    write_fake_gh(
        gh_dir.path(),
        "printf '%s\\n' \"$FAKE_GH_FIRST\" \"$FAKE_GH_SECOND\"\n\
         exit 0\n",
    );
    let output = sandbox.run_with_credential_keyring(
        &["auth", "--gh"],
        credential.description(),
        &[
            ("PATH", gh_path.as_str()),
            ("FAKE_GH_FIRST", GH_INVALID_FIRST),
            ("FAKE_GH_SECOND", GH_INVALID_SECOND),
        ],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_path_clean(gh_dir.path(), &secrets);
    assert_failure(&output, "invalid external gh replacement");
    assert_stderr_contains(
        &output,
        "invalid credential",
        "invalid external gh replacement",
    );
    credential.assert_payload_unchanged(&gh_payload);

    let missing_gh = tempfile::tempdir().expect("empty PATH directory");
    let missing_path = missing_gh.path().to_string_lossy().into_owned();
    let output = sandbox.run_with_credential_keyring(
        &["auth", "--gh"],
        credential.description(),
        &[("PATH", missing_path.as_str())],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_path_clean(missing_gh.path(), &secrets);
    assert_failure(&output, "missing external gh executable");
    assert_stderr_contains(
        &output,
        "install the GitHub CLI",
        "missing external gh executable",
    );
    credential.assert_payload_unchanged(&gh_payload);

    credential.cleanup();
}

#[test]
fn device_flow_persists_but_plan_and_run_never_refresh_or_use_credential() {
    let credential = PersistentCredential::new("device-refresh");
    let sandbox = remote_variable_sandbox();
    let secrets = [DEVICE_ACCESS, DEVICE_REFRESH];

    let oauth = FakeGitHub::bind();
    let oauth_url = oauth.base_url();
    let handle = oauth.serve_recorded(vec![
        Canned::json(
            200,
            "OK",
            r#"{"device_code":"device-capability-code","user_code":"ABCD-EFGH","verification_uri":"https://example.invalid/device","expires_in":900,"interval":0}"#,
        ),
        Canned::json(
            200,
            "OK",
            r#"{"error":"authorization_pending"}"#,
        ),
        Canned::json(
            200,
            "OK",
            format!(
                r#"{{"access_token":"{DEVICE_ACCESS}","refresh_token":"{DEVICE_REFRESH}","expires_in":0,"refresh_token_expires_in":3600}}"#
            ),
        ),
    ]);
    let output = sandbox.run_with_credential_keyring(
        &["auth"],
        credential.description(),
        &[("LITCI_TEST_GITHUB_OAUTH_BASE_URL", oauth_url.as_str())],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_success(&output, "device-flow authentication");
    assert_stdout_contains(
        &output,
        "Credential use remains quarantined until stabilization Phase 16",
        "device-flow authentication",
    );
    let device_payload = credential.capture_payload();
    let requests = handle
        .join()
        .expect("device-flow recording boundary failed");
    assert_request_path(&requests[0], "POST /login/device/code HTTP/1.1");
    assert_request_path(&requests[1], "POST /login/oauth/access_token HTTP/1.1");
    assert_request_path(&requests[2], "POST /login/oauth/access_token HTTP/1.1");

    assert_plan_and_run_quarantine(&sandbox, &credential, &device_payload, &secrets);

    credential.cleanup();
}

fn remote_variable_sandbox() -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write("wf.yml", WORKFLOW);
    sandbox.init_git();
    sandbox.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repo.git",
    ]);
    sandbox
}

fn write_fake_gh(directory: &Path, behavior: &str) {
    let path = directory.join("gh");
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$#\" -ne 2 ] || [ \"$1\" != auth ] || [ \"$2\" != token ]; then\n\
         \t exit 92\n\
         fi\n\
         {behavior}"
    );
    std::fs::write(&path, script).expect("write external gh boundary");
    let mut permissions = std::fs::metadata(&path)
        .expect("inspect external gh boundary")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions).expect("make external gh boundary executable");
}
