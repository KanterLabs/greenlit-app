use crate::credential_capability_support::{assert_clean, assert_failure, assert_stderr_contains};
use crate::support::Sandbox;
use crate::support::fake_github::{Canned, FakeGitHub};

#[test]
fn external_credential_error_bodies_are_sanitized_by_the_compiled_cli() {
    const DEVICE_CODE_BODY_SECRET: &str = "device_code_body_secret_17cb";
    const TOKEN_BODY_ACCESS: &str = "ghu_token_body_access_secret_28dc";
    const TOKEN_BODY_REFRESH: &str = "ghr_token_body_refresh_secret_39ed";
    const UNKNOWN_ERROR_SECRET: &str = "unknown_oauth_error_secret_4afe";
    let secrets = [
        DEVICE_CODE_BODY_SECRET,
        TOKEN_BODY_ACCESS,
        TOKEN_BODY_REFRESH,
        UNKNOWN_ERROR_SECRET,
    ];

    let sandbox = Sandbox::new();
    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    let handle = server.serve(vec![Canned::json(
        200,
        "OK",
        format!(r#"{{"credential":"{DEVICE_CODE_BODY_SECRET}"}}"#),
    )]);
    let output = sandbox.run_with_env(
        &["auth"],
        &[("LITCI_TEST_GITHUB_OAUTH_BASE_URL", base_url.as_str())],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_failure(&output, "malformed device-code response");
    assert_stderr_contains(
        &output,
        "invalid response",
        "malformed device-code response",
    );
    handle.join().expect("device-code error boundary failed");

    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    let handle = server.serve(vec![
        device_code_response(),
        Canned::json(
            200,
            "OK",
            format!(
                r#"{{"access_token":"{TOKEN_BODY_ACCESS}","refresh_token":"{TOKEN_BODY_REFRESH}","malformed": }}"#
            ),
        ),
    ]);
    let output = sandbox.run_with_env(
        &["auth"],
        &[("LITCI_TEST_GITHUB_OAUTH_BASE_URL", base_url.as_str())],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_failure(&output, "malformed access-token response");
    assert_stderr_contains(
        &output,
        "invalid response",
        "malformed access-token response",
    );
    handle.join().expect("token error boundary failed");

    let server = FakeGitHub::bind();
    let base_url = server.base_url();
    let handle = server.serve(vec![
        device_code_response(),
        Canned::json(
            200,
            "OK",
            format!(
                r#"{{"error":"unknown_{UNKNOWN_ERROR_SECRET}","error_description":"{TOKEN_BODY_ACCESS}"}}"#
            ),
        ),
    ]);
    let output = sandbox.run_with_env(
        &["auth"],
        &[("LITCI_TEST_GITHUB_OAUTH_BASE_URL", base_url.as_str())],
    );
    assert_clean(&output, &sandbox, &secrets);
    assert_failure(&output, "unknown device-flow error");
    assert_stderr_contains(
        &output,
        "unrecognized device-flow error",
        "unknown device-flow error",
    );
    handle.join().expect("OAuth error boundary failed");
}

fn device_code_response() -> Canned {
    Canned::json(
        200,
        "OK",
        r#"{"device_code":"device-capability-code","user_code":"ABCD-EFGH","verification_uri":"https://example.invalid/device","expires_in":900,"interval":0}"#,
    )
}
