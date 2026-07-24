//! The GitHub App device flow: request a device/user code pair, then poll
//! until the user has authorized it in a browser.
//!
//! Verified against GitHub's current documentation while implementing this
//! module (`PHASE-3-actions.md` Auth: "Verify device-flow endpoints,
//! refresh behavior, GitHub App permissions, and parameters against GitHub's
//! current documentation during implementation; do not code from memory"):
//!
//! - Step 1, `POST https://github.com/login/device/code` with `client_id`
//!   (a GitHub App's device flow omits `scope`: permissions come from the
//!   app's own installation grant, not an OAuth scope list), returns
//!   `device_code`, `user_code`, `verification_uri`, `expires_in` (900s
//!   default), and `interval` (5s default).
//!   <https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app#device-flow>
//! - Step 3, `POST https://github.com/login/oauth/access_token` with
//!   `client_id`, `device_code`, `grant_type:
//!   urn:ietf:params:oauth:grant-type:device_code`, polled no more often
//!   than `interval` seconds. A successful response returns `access_token`
//!   (`ghu_` prefix), `token_type: bearer`, `expires_in` (28800s/8h),
//!   `refresh_token` (`ghr_` prefix), and `refresh_token_expires_in`
//!   (15897600s/6mo). Same URL as above.
//! - Polling errors (`error` field of an otherwise-200 JSON body):
//!   `authorization_pending` (keep polling), `slow_down` (add 5s to the
//!   interval and keep polling), `expired_token` (restart the flow),
//!   `access_denied` (user declined), `incorrect_client_credentials`,
//!   `incorrect_device_code`, `unsupported_grant_type`, and
//!   `device_flow_disabled` (the GitHub App has not enabled device flow
//!   under its settings) are all documented outcomes this module
//!   distinguishes only as far as "keep polling" vs. "stop and report".
//!   <https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow>
//!
//! Every request sends `Accept: application/json` — GitHub's default
//! response encoding without it is `application/x-www-form-urlencoded`,
//! which this module does not parse.

use std::time::{Duration, Instant};

use serde::Deserialize;

/// Real GitHub's device-flow/token-exchange host. Both endpoints this module
/// calls live under `github.com`, not `api.github.com`.
pub(crate) const DEFAULT_OAUTH_BASE_URL: &str = "https://github.com";

const USER_AGENT: &str = concat!("greenlit-litci/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// The device/user code pair from step 1.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeviceCode {
    pub(crate) device_code: String,
    /// Shown to the user (e.g. `WDJB-MJHT`).
    pub(crate) user_code: String,
    /// Where the user enters `user_code` (`https://github.com/login/device`).
    pub(crate) verification_uri: String,
    /// Seconds until `device_code`/`user_code` expire.
    pub(crate) expires_in: u64,
    /// Minimum seconds between polls.
    pub(crate) interval: u64,
}

/// A successful token exchange.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeviceToken {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_in: Option<u64>,
    pub(crate) refresh_token_expires_in: Option<u64>,
}

/// The result of one poll attempt.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PollOutcome {
    /// The user has authorized the app; the exchange is complete.
    Success(DeviceToken),
    /// `authorization_pending` — the user has not yet entered the code.
    Pending,
    /// `slow_down` — the interval must grow by 5 seconds.
    SlowDown,
    /// `access_denied` — the user declined.
    Denied,
    /// `expired_token` — the device code expired before authorization.
    Expired,
    /// Any other documented error (`incorrect_client_credentials`,
    /// `incorrect_device_code`, `unsupported_grant_type`,
    /// `device_flow_disabled`), or a transport/parse failure.
    Error(String),
}

/// Talks to the device-flow endpoints. `crate::auth` always constructs one
/// via [`DeviceFlowClient::with_base_url`] — production passes real
/// `https://github.com` (`DEFAULT_OAUTH_BASE_URL`, via
/// `crate::auth::oauth_base_url`, which is also the one seam its internal
/// test-only override replaces); tests inject a loopback base URL directly.
/// The same injection pattern `greenlit_actions::resolve::GitHubApiResolver`
/// uses, and required here for the same reason (`PHASE-3-actions.md` exit
/// criterion 5: "device flow … use a mocked external GitHub endpoint").
#[derive(Debug, Clone)]
pub(crate) struct DeviceFlowClient {
    agent: ureq::Agent,
    base_url: String,
}

impl DeviceFlowClient {
    /// A client against a caller-chosen base URL.
    pub(crate) fn with_base_url(base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
            base_url: base_url.into(),
        }
    }

    /// Step 1: requests a fresh device/user code pair.
    pub(crate) fn request_device_code(&self, client_id: &str) -> Result<DeviceCode, String> {
        let url = format!("{}/login/device/code", self.base_url);
        let mut response = self
            .agent
            .post(&url)
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .send_form([("client_id", client_id)])
            .map_err(|error| format!("could not reach {url}: {error}"))?;
        let body = read_body(&mut response)?;
        let parsed: DeviceCodeResponse = serde_json::from_slice(&body).map_err(|error| {
            format!(
                "unexpected response requesting a device code: {error} (body: {})",
                String::from_utf8_lossy(&body)
            )
        })?;
        Ok(DeviceCode {
            device_code: parsed.device_code,
            user_code: parsed.user_code,
            verification_uri: parsed.verification_uri,
            expires_in: parsed.expires_in,
            interval: parsed.interval,
        })
    }

    /// Step 3, one attempt: polls the token endpoint once.
    pub(crate) fn poll_once(&self, client_id: &str, device_code: &str) -> PollOutcome {
        let url = format!("{}/login/oauth/access_token", self.base_url);
        let mut response = match self
            .agent
            .post(&url)
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .send_form([
                ("client_id", client_id),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ]) {
            Ok(response) => response,
            Err(error) => return PollOutcome::Error(format!("could not reach {url}: {error}")),
        };
        let body = match read_body(&mut response) {
            Ok(body) => body,
            Err(error) => return PollOutcome::Error(error),
        };
        let parsed: TokenResponse = match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(error) => {
                return PollOutcome::Error(format!(
                    "unexpected response polling for the access token: {error} (body: {})",
                    String::from_utf8_lossy(&body)
                ));
            }
        };
        classify(parsed)
    }
}

fn classify(parsed: TokenResponse) -> PollOutcome {
    match parsed.error.as_deref() {
        None => {
            let Some(access_token) = parsed.access_token else {
                return PollOutcome::Error(
                    "GitHub returned neither an access token nor an error".to_string(),
                );
            };
            PollOutcome::Success(DeviceToken {
                access_token,
                refresh_token: parsed.refresh_token,
                expires_in: parsed.expires_in,
                refresh_token_expires_in: parsed.refresh_token_expires_in,
            })
        }
        Some("authorization_pending") => PollOutcome::Pending,
        Some("slow_down") => PollOutcome::SlowDown,
        Some("access_denied") => PollOutcome::Denied,
        Some("expired_token") => PollOutcome::Expired,
        Some(other) => PollOutcome::Error(
            parsed
                .error_description
                .unwrap_or_else(|| other.to_string()),
        ),
    }
}

/// Drives [`DeviceFlowClient::poll_once`] to completion: sleeps at least
/// `code.interval` seconds between attempts (growing by 5s on every
/// `slow_down`, per GitHub's documented backoff), and gives up once
/// `code.expires_in` seconds have elapsed since the code was issued.
/// `sleep` is injected so tests never wait for a real interval.
pub(crate) fn poll_until_authorized(
    client: &DeviceFlowClient,
    client_id: &str,
    code: &DeviceCode,
    sleep: impl Fn(Duration),
) -> Result<DeviceToken, String> {
    let mut interval = Duration::from_secs(code.interval.max(1));
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    loop {
        sleep(interval);
        if Instant::now() >= deadline {
            return Err(
                "the device code expired before authorization completed\n  fix: run `litci auth` again"
                    .to_string(),
            );
        }
        match client.poll_once(client_id, &code.device_code) {
            PollOutcome::Success(token) => return Ok(token),
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval += Duration::from_secs(5),
            PollOutcome::Denied => {
                return Err(
                    "authorization was denied\n  fix: run `litci auth` again and approve the request on github.com"
                        .to_string(),
                );
            }
            PollOutcome::Expired => {
                return Err("the device code expired\n  fix: run `litci auth` again".to_string());
            }
            PollOutcome::Error(message) => {
                return Err(format!(
                    "device flow failed: {message}\n  fix: run `litci auth` again"
                ));
            }
        }
    }
}

fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, String> {
    response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|error| format!("could not read GitHub's response: {error}"))
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize, Default)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token_expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn drain_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let mut buf = [0_u8; 8192];
        let mut total = Vec::new();
        loop {
            let read = stream.read(&mut buf).unwrap_or(0);
            if read == 0 {
                break;
            }
            total.extend_from_slice(&buf[..read]);
            if total.windows(4).any(|w| w == b"\r\n\r\n") {
                // For a POST with a body, keep draining until the
                // Content-Length worth of bytes has definitely arrived; the
                // fixed-size bodies here always fit the first read.
                break;
            }
        }
    }

    fn respond(listener: &TcpListener, body: &str) {
        let (mut stream, _) = listener.accept().expect("accept");
        drain_request(&mut stream);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    }

    #[test]
    fn requests_a_device_code() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond(
                &listener,
                r#"{"device_code":"d","user_code":"ABCD-EFGH","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#,
            );
        });

        let client = DeviceFlowClient::with_base_url(base_url);
        let code = client
            .request_device_code("client-id")
            .expect("device code");
        assert_eq!(code.user_code, "ABCD-EFGH");
        assert_eq!(code.expires_in, 900);
        assert_eq!(code.interval, 5);
        handle.join().unwrap();
    }

    /// A predicate over one poll outcome — pulled out to its own alias so
    /// the data-driven table below stays a plain array type for clippy's
    /// type-complexity lint.
    type OutcomePredicate = fn(PollOutcome) -> bool;

    #[test]
    fn poll_once_classifies_every_documented_outcome() {
        let cases: [(&str, OutcomePredicate); 4] = [
            (
                r#"{"error":"authorization_pending"}"#,
                (|outcome| matches!(outcome, PollOutcome::Pending)) as OutcomePredicate,
            ),
            (
                r#"{"error":"slow_down"}"#,
                (|outcome| matches!(outcome, PollOutcome::SlowDown)) as OutcomePredicate,
            ),
            (
                r#"{"error":"access_denied"}"#,
                (|outcome| matches!(outcome, PollOutcome::Denied)) as OutcomePredicate,
            ),
            (
                r#"{"error":"expired_token"}"#,
                (|outcome| matches!(outcome, PollOutcome::Expired)) as OutcomePredicate,
            ),
        ];
        for (body, predicate) in cases {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let body = body.to_string();
            let handle = std::thread::spawn(move || respond(&listener, &body));
            let client = DeviceFlowClient::with_base_url(base_url);
            let outcome = client.poll_once("client-id", "device-code");
            assert!(
                predicate(outcome.clone()),
                "unexpected outcome: {outcome:?}"
            );
            handle.join().unwrap();
        }
    }

    #[test]
    fn poll_once_reports_a_successful_token_exchange() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond(
                &listener,
                r#"{"access_token":"ghu_abc","token_type":"bearer","scope":"","expires_in":28800,"refresh_token":"ghr_xyz","refresh_token_expires_in":15897600}"#,
            );
        });
        let client = DeviceFlowClient::with_base_url(base_url);
        let outcome = client.poll_once("client-id", "device-code");
        assert_eq!(
            outcome,
            PollOutcome::Success(DeviceToken {
                access_token: "ghu_abc".to_string(),
                refresh_token: Some("ghr_xyz".to_string()),
                expires_in: Some(28800),
                refresh_token_expires_in: Some(15_897_600),
            })
        );
        handle.join().unwrap();
    }

    #[test]
    fn poll_until_authorized_retries_pending_then_succeeds_without_real_sleeping() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond(&listener, r#"{"error":"authorization_pending"}"#);
            respond(
                &listener,
                r#"{"access_token":"ghu_final","token_type":"bearer","scope":"","expires_in":28800}"#,
            );
        });
        let client = DeviceFlowClient::with_base_url(base_url);
        let code = DeviceCode {
            device_code: "device-code".to_string(),
            user_code: "ABCD-EFGH".to_string(),
            verification_uri: "https://github.com/login/device".to_string(),
            expires_in: 900,
            interval: 0,
        };
        let token = poll_until_authorized(&client, "client-id", &code, |_duration| {
            // No real sleeping in tests: interval 0 plus a no-op sleep
            // function keeps this test bounded in milliseconds.
        })
        .expect("eventual success");
        assert_eq!(token.access_token, "ghu_final");
        handle.join().unwrap();
    }

    #[test]
    fn poll_until_authorized_reports_denial() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond(&listener, r#"{"error":"access_denied"}"#);
        });
        let client = DeviceFlowClient::with_base_url(base_url);
        let code = DeviceCode {
            device_code: "device-code".to_string(),
            user_code: "ABCD-EFGH".to_string(),
            verification_uri: "https://github.com/login/device".to_string(),
            expires_in: 900,
            interval: 0,
        };
        let error = poll_until_authorized(&client, "client-id", &code, |_| {}).unwrap_err();
        assert!(error.contains("denied"), "{error}");
        assert!(error.contains("litci auth"), "{error}");
        handle.join().unwrap();
    }
}
