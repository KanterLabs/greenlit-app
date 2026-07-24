//! `litci auth`: GitHub App device-flow login, PAT paste, and `gh` CLI
//! passthrough, plus transparent token storage/refresh consumed by the
//! variables (`crate::vars`) and secrets (`crate::secrets`) resolution
//! chains.
//!
//! `PHASE-3-actions.md` Auth: device flow using the embedded public client
//! ID, `--pat`, `--gh`, keyring-first storage, and transparent refresh on
//! expiry. See `crates/greenlit-app/src/auth/device_flow.rs` for the
//! doc-verified device-flow wire details and `refresh.rs` for the refresh
//! exchange.

mod device_flow;
mod gh_cli;
mod pat;
mod permission_notice;
mod refresh;
mod token_store;

pub(crate) use permission_notice::token_permission_notice;

use std::path::PathBuf;

use greenlit_expr::Value;

use token_store::{StoredToken, TokenSource};

/// The `greenlit-app` GitHub App's public client ID, embedded per
/// `AGENTS.md`'s Owner-provided-input table and `PHASE-3-actions.md` Auth
/// ("GitHub App device flow using the supplied public client ID
/// `Iv23liyZuAdn5DSMxtyh`").
pub(crate) const GITHUB_APP_CLIENT_ID: &str = "Iv23liyZuAdn5DSMxtyh";

/// Internal test-only override: forces every credential-store access in
/// this process to skip the kernel keyring, using only the `0600` file
/// fallback. See `crate::auth::token_store`'s module doc comment for why —
/// the kernel keyring is scoped to the real test-runner process UID, not to
/// the sandboxed `$HOME` the integration tests otherwise isolate everything
/// through, so exercising it for real from an automated test would read or
/// write the *actual* developer/CI account's persistent keyring. Not a
/// documented user-facing flag.
const TEST_NO_KEYRING_ENV: &str = "LITCI_TEST_NO_KEYRING";

/// Internal test-only override for the device-flow/token-exchange base URL
/// (`https://github.com` in production). Lets the compiled-binary
/// integration tests point `litci auth` at a loopback fake without any
/// production configuration surface.
const TEST_OAUTH_BASE_URL_ENV: &str = "LITCI_TEST_GITHUB_OAUTH_BASE_URL";

fn allow_keyring() -> bool {
    std::env::var_os(TEST_NO_KEYRING_ENV).is_none()
}

fn oauth_base_url() -> String {
    std::env::var(TEST_OAUTH_BASE_URL_ENV)
        .unwrap_or_else(|_| device_flow::DEFAULT_OAUTH_BASE_URL.to_string())
}

/// Resolves the user-local state directory (`~/.litci`'s parent, i.e.
/// `$HOME`) the same way `greenlit-metrics` does — see that crate's
/// `MetricsStore::open_default` doc comment for why no cross-platform
/// home-directory crate is used.
fn home_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        "could not determine the user home directory (HOME is not set)\n  fix: set HOME, then retry"
            .to_string()
    })?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(
            "the HOME environment variable is not an absolute path\n  fix: set HOME to an absolute directory, then retry"
                .to_string(),
        );
    }
    Ok(home)
}

/// The currently valid access token, if any, refreshing transparently when
/// the stored token is a device-flow token past its known expiry.
///
/// Returns `Ok(None)` for "not authenticated" (never stored, or a refresh
/// that itself failed/expired) — every caller treats that identically to
/// having never called `litci auth`, pointing back at it as the fix. This
/// is the single entry point `crate::vars`/`crate::secrets` (and, in a
/// later phase, action resolution's `GitHubApiResolver`/`RefResolver`
/// construction) call to obtain a token without any of them needing to know
/// which backend it came from or whether a refresh just happened.
pub(crate) fn current_token() -> Result<Option<String>, String> {
    let home = home_dir()?;
    let Some(stored) = token_store::load(&home, allow_keyring()) else {
        return Ok(None);
    };
    let needs_refresh = stored
        .access_token_expires_at
        .is_some_and(token_store::is_expired);
    if !needs_refresh {
        return Ok(Some(stored.access_token));
    }
    let Some(refresh_token) = stored.refresh_token.clone() else {
        // No refresh token (a PAT/`gh` token past a *known* expiry never
        // happens today — those sources never set an expiry — but stay
        // total): fall back to using the possibly-stale access token and
        // let the API itself report the auth failure.
        return Ok(Some(stored.access_token));
    };
    if stored
        .refresh_token_expires_at
        .is_some_and(token_store::is_expired)
    {
        return Ok(None);
    }
    match refresh::refresh_access_token(&oauth_base_url(), GITHUB_APP_CLIENT_ID, &refresh_token) {
        Ok(refreshed) => {
            let updated = StoredToken {
                access_token: refreshed.access_token.clone(),
                refresh_token: Some(refreshed.refresh_token),
                access_token_expires_at: Some(token_store::expires_in(refreshed.expires_in)),
                refresh_token_expires_at: Some(token_store::expires_in(
                    refreshed.refresh_token_expires_in,
                )),
                source: TokenSource::DeviceFlow,
            };
            let _ = token_store::save(&home, &updated, allow_keyring());
            Ok(Some(refreshed.access_token))
        }
        // A refresh failure means re-authentication is required; the stale
        // access token is not returned since it is already known-expired.
        Err(_) => Ok(None),
    }
}

/// Resolves `secrets.GITHUB_TOKEN`/`github.token` for a workflow that
/// references either. `local_override` is the value from the ordinary
/// `-s`/env/`.litci/secrets` chain for the literal name `GITHUB_TOKEN`
/// (`crate::secrets::local_override_for`), ranked above the stored auth
/// token — the same "local overrides remote" precedence every other
/// resolution chain in this tool uses.
///
/// Never fails the run: an unresolved token (nothing local, nobody
/// authenticated) resolves to an empty string, mirroring the vars chain's
/// GitHub-parity rule that an absent name resolves to `""` rather than
/// stopping — `PHASE-3-actions.md` only requires *variable* lookups to stop
/// before planning on missing auth, since (unlike a variable) GitHub itself
/// always provides a working `GITHUB_TOKEN`, so v0 degrading gracefully to
/// "no token" for the *host-simulated* case is closer to observed
/// expectations than blocking every workflow that merely references it.
pub(crate) fn resolve_github_token(local_override: Option<String>) -> (String, Option<String>) {
    if let Some(value) = local_override {
        return (value, None);
    }
    match current_token() {
        Ok(Some(token)) => (token, None),
        Ok(None) => (
            String::new(),
            Some(
                "note: this workflow references the GitHub token (`secrets.GITHUB_TOKEN`/`github.token`) but no local token is configured, so it resolves to an empty string\n  fix: run `litci auth` (or `litci auth --pat`/`--gh`) to supply one"
                    .to_string(),
            ),
        ),
        Err(message) => (
            String::new(),
            Some(format!(
                "note: could not obtain the stored GitHub token, so `secrets.GITHUB_TOKEN`/`github.token` resolves to an empty string: {message}\n  fix: run `litci auth` again"
            )),
        ),
    }
}

/// Runs the device-flow login: prints the user code and verification URL,
/// polls until authorized (or the code expires/is denied), and stores the
/// resulting token.
pub(crate) fn run_device_flow(out: &mut impl std::io::Write) -> Result<(), String> {
    let client = device_flow::DeviceFlowClient::with_base_url(oauth_base_url());
    let code = client
        .request_device_code(GITHUB_APP_CLIENT_ID)
        .map_err(|error| format!("{error}\n  fix: check network connectivity, then retry"))?;
    writeln!(out, "First, copy your one-time code: {}", code.user_code).ok();
    writeln!(
        out,
        "Then open {} in a browser and enter it.",
        code.verification_uri
    )
    .ok();
    writeln!(out, "Waiting for authorization…").ok();
    let token = device_flow::poll_until_authorized(&client, GITHUB_APP_CLIENT_ID, &code, |d| {
        std::thread::sleep(d)
    })?;
    let stored = StoredToken {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        access_token_expires_at: token.expires_in.map(token_store::expires_in),
        refresh_token_expires_at: token.refresh_token_expires_in.map(token_store::expires_in),
        source: TokenSource::DeviceFlow,
    };
    store_and_report(out, &stored)
}

/// Runs `litci auth --pat`: prompts (no echo) for a pasted fine-grained
/// personal access token and stores it.
pub(crate) fn run_pat_flow(out: &mut impl std::io::Write) -> Result<(), String> {
    pat::print_permission_guidance(out);
    let token = pat::prompt_for_pat()?;
    let stored = StoredToken {
        access_token: token,
        refresh_token: None,
        access_token_expires_at: None,
        refresh_token_expires_at: None,
        source: TokenSource::Pat,
    };
    store_and_report(out, &stored)
}

/// Runs `litci auth --gh`: reads `gh auth token` and stores it verbatim,
/// with a broad-scope warning (a `gh`-issued token typically carries
/// whatever scopes the user originally granted `gh` itself, which is very
/// unlikely to be limited to read-only contents/variables access).
pub(crate) fn run_gh_flow(out: &mut impl std::io::Write) -> Result<(), String> {
    let token = gh_cli::token_from_gh()?;
    writeln!(
        out,
        "Warning: this token comes from `gh auth token` and likely carries broader scopes than Greenlit needs (read-only repository contents/variables). Anything that token can do, a workflow step that references it can do too."
    )
    .ok();
    let stored = StoredToken {
        access_token: token,
        refresh_token: None,
        access_token_expires_at: None,
        refresh_token_expires_at: None,
        source: TokenSource::Gh,
    };
    store_and_report(out, &stored)
}

fn store_and_report(out: &mut impl std::io::Write, stored: &StoredToken) -> Result<(), String> {
    let home = home_dir()?;
    // Replace any previous credential outright so a stale keyring entry and
    // a freshly written file (or vice versa) can never both resolve.
    token_store::clear(&home, allow_keyring());
    let backend = token_store::save(&home, stored, allow_keyring())
        .map_err(|error| format!("could not store the credential: {error}"))?;
    match backend {
        token_store::StoreBackend::Keyring => {
            writeln!(out, "Stored the token in the system keyring.").ok();
        }
        token_store::StoreBackend::File => {
            writeln!(
                out,
                "Warning: the system keyring was unavailable, so the token was written to ~/.litci/auth.json (mode 0600) instead. Anyone who can read that file as you can read the token."
            )
            .ok();
        }
    }
    writeln!(
        out,
        "Authenticated. `litci run`/`litci plan` will now use this token for GitHub lookups."
    )
    .ok();
    Ok(())
}

/// Injects `token` as `github.token` into an already-built `github` context
/// object, for a workflow that references it
/// (`greenlit_workflow::extract::StaticExtraction::references_github_token`).
/// Host-side lookup and action-source fetching never call this — only
/// `crate::run_cmd`, and only when the reference actually exists
/// (`PHASE-3-actions.md` Auth: "Inject `GITHUB_TOKEN`/`github.token` only
/// into workflows that reference them").
pub(crate) fn inject_github_token(github: Value, token: &str) -> Value {
    match github {
        Value::Object(object) => {
            let mut entries: Vec<(String, Value)> = object
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect();
            entries.push(("token".to_string(), Value::String(token.to_string())));
            Value::object(entries)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greenlit_expr::Value;

    #[test]
    fn inject_github_token_adds_a_token_field_without_disturbing_others() {
        let github = Value::object(vec![
            ("event_name".to_string(), Value::String("push".to_string())),
            ("repository".to_string(), Value::String("o/r".to_string())),
        ]);
        let injected = inject_github_token(github, "ghu_abc");
        let Value::Object(object) = injected else {
            panic!("expected object");
        };
        assert_eq!(
            object.get("token"),
            Some(&Value::String("ghu_abc".to_string()))
        );
        assert_eq!(
            object.get("event_name"),
            Some(&Value::String("push".to_string()))
        );
    }
}
