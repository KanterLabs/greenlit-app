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
    error_description: Option<String>,
}

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
        .map_err(|error| format!("could not reach {url}: {error}"))?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|error| format!("could not read GitHub's refresh response: {error}"))?;
    let parsed: RefreshResponse = serde_json::from_slice(&body).map_err(|error| {
        format!(
            "unexpected response refreshing the token: {error} (body: {})",
            String::from_utf8_lossy(&body)
        )
    })?;
    if let Some(error) = parsed.error {
        return Err(parsed.error_description.unwrap_or(error));
    }
    let access_token = parsed
        .access_token
        .ok_or_else(|| "GitHub's refresh response had no access_token".to_string())?;
    let refresh_token = parsed
        .refresh_token
        .ok_or_else(|| "GitHub's refresh response had no refresh_token".to_string())?;
    Ok(RefreshedToken {
        access_token,
        refresh_token,
        expires_in: parsed.expires_in.unwrap_or(28_800),
        refresh_token_expires_in: parsed.refresh_token_expires_in.unwrap_or(15_897_600),
    })
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
                break;
            }
        }
    }

    fn respond(listener: TcpListener, body: &'static str) {
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
    fn refreshes_and_rotates_both_tokens() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond(
                listener,
                r#"{"access_token":"ghu_new","token_type":"bearer","scope":"","expires_in":28800,"refresh_token":"ghr_new","refresh_token_expires_in":15897600}"#,
            );
        });

        let refreshed = refresh_access_token(&base_url, "client-id", "ghr_old").expect("refresh");
        assert_eq!(refreshed.access_token, "ghu_new");
        assert_eq!(refreshed.refresh_token, "ghr_new");
        assert_eq!(refreshed.expires_in, 28_800);
        handle.join().unwrap();
    }

    #[test]
    fn an_error_response_surfaces_its_description() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond(
                listener,
                r#"{"error":"bad_refresh_token","error_description":"The refresh token is invalid or expired."}"#,
            );
        });

        let error = refresh_access_token(&base_url, "client-id", "ghr_old").unwrap_err();
        assert!(error.contains("invalid or expired"), "{error}");
        handle.join().unwrap();
    }
}
