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
/// `crate::auth::oauth_base_url`); only an explicit
/// `litci_test_boundaries` custom-cfg build can replace that URL with a
/// loopback boundary.
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
            .map_err(|_| "could not reach GitHub's device-code endpoint".to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "GitHub's device-code endpoint returned HTTP {}",
                status.as_u16()
            ));
        }
        let body = read_body(&mut response)?;
        let parsed: DeviceCodeResponse = serde_json::from_slice(&body).map_err(|_| {
            format!(
                "GitHub's device-code endpoint returned HTTP {} with an invalid response",
                status.as_u16()
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
            Err(_) => {
                return PollOutcome::Error(
                    "could not reach GitHub's access-token endpoint".to_string(),
                );
            }
        };
        let status = response.status();
        if !status.is_success() {
            return PollOutcome::Error(format!(
                "GitHub's access-token endpoint returned HTTP {}",
                status.as_u16()
            ));
        }
        let body = match read_body(&mut response) {
            Ok(body) => body,
            Err(error) => return PollOutcome::Error(error),
        };
        let parsed: TokenResponse = match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(_) => {
                return PollOutcome::Error(format!(
                    "GitHub's access-token endpoint returned HTTP {} with an invalid response",
                    status.as_u16()
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
                    "GitHub's access-token endpoint returned an incomplete response".to_string(),
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
        Some("incorrect_client_credentials") => {
            PollOutcome::Error("GitHub rejected the OAuth client credentials".to_string())
        }
        Some("incorrect_device_code") => {
            PollOutcome::Error("GitHub rejected the device code".to_string())
        }
        Some("unsupported_grant_type") => {
            PollOutcome::Error("GitHub rejected the device-flow grant type".to_string())
        }
        Some("device_flow_disabled") => {
            PollOutcome::Error("GitHub App device flow is disabled".to_string())
        }
        Some(_) => {
            PollOutcome::Error("GitHub returned an unrecognized device-flow error".to_string())
        }
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
        .map_err(|_| "could not read GitHub's OAuth response".to_string())
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
}
