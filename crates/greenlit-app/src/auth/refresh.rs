//! Refreshing an expired GitHub App user access token.
//!
//! Verified against GitHub's current documentation while implementing this
//! module (`PHASE-3-actions.md` Auth: "refresh transparently on expiry"):
//! `POST https://github.com/login/oauth/access_token` with `client_id`,
//! `grant_type: refresh_token`, and `refresh_token` (`client_secret` is
//! documented as "required unless tokens were generated via device flow" —
//! exactly this crate's case, since `litci auth`'s embedded client ID is a
//! public device-flow client with no secret to hold). A successful response
//! returns a new `access_token` (`ghu_` prefix, `expires_in: 28800` = 8h)
//! and a new `refresh_token` (`ghr_` prefix, `refresh_token_expires_in:
//! 15897600` = 6mo) — GitHub always rotates the refresh token, so the
//! caller must persist the new one, not the one it exchanged.
//! <https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/refreshing-user-access-tokens>

use std::time::Duration;

use serde::Deserialize;

const USER_AGENT: &str = concat!("greenlit-litci/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// A successful refresh.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RefreshedToken {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_in: u64,
    pub(crate) refresh_token_expires_in: u64,
}

#[derive(Debug, Deserialize, Default)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token_expires_in: Option<u64>,
    error: Option<String>,
}

const REAUTH_FIX: &str = "\n  fix: run `litci auth` again";

/// Exchanges `refresh_token` for a new access/refresh token pair against
/// `base_url` (real GitHub, or a loopback fake in tests).
pub(crate) fn refresh_access_token(
    base_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<RefreshedToken, String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = config.into();
    let url = format!("{base_url}/login/oauth/access_token");
    let mut response = agent
        .post(&url)
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .send_form([
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .map_err(|_| refresh_error("could not reach GitHub's token-refresh endpoint"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(refresh_error(&format!(
            "GitHub's token-refresh endpoint returned HTTP {}",
            status.as_u16()
        )));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|_| refresh_error("could not read GitHub's token-refresh response"))?;
    let parsed: RefreshResponse = serde_json::from_slice(&body).map_err(|_| {
        refresh_error(&format!(
            "GitHub's token-refresh endpoint returned HTTP {} with an invalid response",
            status.as_u16()
        ))
    })?;
    if parsed.error.is_some() {
        return Err(refresh_error("GitHub rejected the refresh credential"));
    }
    let access_token = parsed.access_token.ok_or_else(|| {
        refresh_error("GitHub's token-refresh endpoint returned an incomplete response")
    })?;
    let refresh_token = parsed.refresh_token.ok_or_else(|| {
        refresh_error("GitHub's token-refresh endpoint returned an incomplete response")
    })?;
    Ok(RefreshedToken {
        access_token,
        refresh_token,
        expires_in: parsed.expires_in.unwrap_or(28_800),
        refresh_token_expires_in: parsed.refresh_token_expires_in.unwrap_or(15_897_600),
    })
}

fn refresh_error(category: &str) -> String {
    format!("{category}{REAUTH_FIX}")
}
