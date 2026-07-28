//! Capability-owned compiled-CLI acceptance for production credential
//! persistence. This target is run only by a Linux job that provisions an
//! isolated persistent keyring; missing prerequisites and cleanup failures
//! are hard failures.

#[path = "credential_capability/keyring_support.rs"]
mod credential_capability_support;
#[path = "credential_capability/sanitization.rs"]
mod sanitization;
pub mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use credential_capability_support::{
    PersistentCredential, assert_bearer, assert_clean, assert_failure, assert_form_value,
    assert_path_clean, assert_request_path, assert_stderr_contains, assert_stdout_contains,
    assert_success,
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
const REFRESH_ERROR_ACCESS: &str = "ghu_refresh_error_access_2c87";
const REFRESH_ERROR_REFRESH: &str = "ghr_refresh_error_refresh_3d98";
const REFRESHED_ACCESS: &str = "ghu_refreshed_access_secret_4ea9";
const REFRESHED_REFRESH: &str = "ghr_refreshed_refresh_secret_5fba";

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
    credential.assert_present();
    assert_plan_uses_token(&sandbox, credential.description(), PAT_TOKEN, &secrets, &[]);

    let output = sandbox.run_with_credential_keyring_stdin(
        &["auth", "--pat"],
        credential.description(),
        &[],
        &format!("{oversized}\n"),
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_failure(&output, "oversized PAT replacement");
    assert_stderr_contains(&output, "keyring limit", "oversized PAT replacement");
    assert_plan_uses_token(&sandbox, credential.description(), PAT_TOKEN, &secrets, &[]);

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
    assert_plan_uses_token(&sandbox, credential.description(), GH_TOKEN, &secrets, &[]);

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
    assert_plan_uses_token(&sandbox, credential.description(), GH_TOKEN, &secrets, &[]);

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
    assert_plan_uses_token(&sandbox, credential.description(), GH_TOKEN, &secrets, &[]);

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
    assert_plan_uses_token(&sandbox, credential.description(), GH_TOKEN, &secrets, &[]);

    credential.cleanup();
}

#[test]
fn device_flow_refresh_rotates_and_persists_across_processes_without_leaking() {
    let credential = PersistentCredential::new("device-refresh");
    let sandbox = remote_variable_sandbox();
    let secrets = [
        DEVICE_ACCESS,
        DEVICE_REFRESH,
        REFRESH_ERROR_ACCESS,
        REFRESH_ERROR_REFRESH,
        REFRESHED_ACCESS,
        REFRESHED_REFRESH,
    ];

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
    assert_stdout_contains(&output, "Authenticated", "device-flow authentication");
    credential.assert_present();
    let requests = handle
        .join()
        .expect("device-flow recording boundary failed");
    assert_request_path(&requests[0], "POST /login/device/code HTTP/1.1");
    assert_request_path(&requests[1], "POST /login/oauth/access_token HTTP/1.1");
    assert_request_path(&requests[2], "POST /login/oauth/access_token HTTP/1.1");

    let refresh_error = FakeGitHub::bind();
    let refresh_error_url = refresh_error.base_url();
    let handle = refresh_error.serve_recorded(vec![Canned::json(
        200,
        "OK",
        format!(
            r#"{{"error":"bad_refresh","access_token":"{REFRESH_ERROR_ACCESS}","refresh_token":"{REFRESH_ERROR_REFRESH}"}}"#
        ),
    )]);
    let output = sandbox.run_with_credential_keyring(
        &["plan", "-W", "wf.yml"],
        credential.description(),
        &[
            (
                "LITCI_TEST_GITHUB_OAUTH_BASE_URL",
                refresh_error_url.as_str(),
            ),
            ("LITCI_TEST_GITHUB_API_BASE_URL", "http://127.0.0.1:9"),
        ],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_failure(&output, "rejected refresh");
    let requests = handle.join().expect("refresh error boundary failed");
    assert_form_value(&requests[0], "refresh_token", DEVICE_REFRESH);

    let refresh = FakeGitHub::bind();
    let refresh_url = refresh.base_url();
    let refresh_handle = refresh.serve_recorded(vec![Canned::json(
        200,
        "OK",
        format!(
            r#"{{"access_token":"{REFRESHED_ACCESS}","refresh_token":"{REFRESHED_REFRESH}","expires_in":3600,"refresh_token_expires_in":7200}}"#
        ),
    )]);
    let api = FakeGitHub::bind();
    let api_url = api.base_url();
    let api_handle = api.serve_recorded(vec![variable_response()]);
    let output = sandbox.run_with_credential_keyring(
        &["plan", "-W", "wf.yml"],
        credential.description(),
        &[
            ("LITCI_TEST_GITHUB_OAUTH_BASE_URL", refresh_url.as_str()),
            ("LITCI_TEST_GITHUB_API_BASE_URL", api_url.as_str()),
        ],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_success(&output, "refreshing plan");
    let refresh_requests = refresh_handle.join().expect("refresh boundary failed");
    assert_form_value(&refresh_requests[0], "refresh_token", DEVICE_REFRESH);
    let api_requests = api_handle.join().expect("API recording boundary failed");
    assert_bearer(&api_requests[0], REFRESHED_ACCESS);

    assert_plan_uses_token(
        &sandbox,
        credential.description(),
        REFRESHED_ACCESS,
        &secrets,
        &[("LITCI_TEST_GITHUB_OAUTH_BASE_URL", "http://127.0.0.1:9")],
    );

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

fn assert_plan_uses_token(
    sandbox: &Sandbox,
    description: &str,
    expected_token: &str,
    secrets: &[&str],
    extra_env: &[(&str, &str)],
) {
    let api = FakeGitHub::bind();
    let api_url = api.base_url();
    let handle = api.serve_recorded(vec![variable_response()]);
    let mut env = Vec::with_capacity(extra_env.len() + 1);
    env.extend_from_slice(extra_env);
    env.push(("LITCI_TEST_GITHUB_API_BASE_URL", api_url.as_str()));
    let output = sandbox.run_with_credential_keyring(&["plan", "-W", "wf.yml"], description, &env);
    assert_clean(&output, sandbox, secrets);
    assert_success(&output, "cross-process credential load");
    let requests = handle.join().expect("API recording boundary failed");
    assert_request_path(
        &requests[0],
        "GET /repos/owner/repo/actions/variables/MODE HTTP/1.1",
    );
    assert_bearer(&requests[0], expected_token);
}

fn variable_response() -> Canned {
    Canned::json(200, "OK", r#"{"name":"MODE","value":"ci"}"#)
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
