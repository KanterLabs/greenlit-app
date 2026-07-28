//! `litci auth`: GitHub App device-flow login, PAT paste, and `gh` CLI
//! passthrough and credential storage. Phase 12 deliberately exposes no
//! production credential-loading API: selected workflows are quarantined
//! before credential retrieval until Phase 16 certifies the trust boundary.
//!
//! `PHASE-3-actions.md` Auth: device flow using the embedded public client
//! ID, `--pat`, and `--gh`. See `crates/greenlit-app/src/auth/device_flow.rs`
//! for the doc-verified device-flow wire details.

mod device_flow;
mod gh_cli;
mod pat;
mod token_store;

use std::path::PathBuf;

use token_store::{StoredToken, TokenSource};

/// The `greenlit-app` GitHub App's public client ID, embedded per
/// `AGENTS.md`'s Owner-provided-input table and `PHASE-3-actions.md` Auth
/// ("GitHub App device flow using the supplied public client ID
/// `Iv23liyZuAdn5DSMxtyh`").
pub(crate) const GITHUB_APP_CLIENT_ID: &str = "Iv23liyZuAdn5DSMxtyh";

/// Internal test-boundary override: forces every credential-store access in
/// this process to report the kernel keyring as unavailable. This constant
/// and its reader do not exist in ordinary or release builds.
#[cfg(litci_test_boundaries)]
const TEST_NO_KEYRING_ENV: &str = "LITCI_TEST_NO_KEYRING";

/// Internal test-boundary override used only by the credential capability
/// target. The capability retains the production keyring syscalls while
/// isolating fake credentials under a non-production description.
#[cfg(litci_test_boundaries)]
const TEST_KEYRING_DESCRIPTION_ENV: &str = "LITCI_TEST_KEYRING_DESCRIPTION";

/// Internal test-boundary override for the OAuth base URL. Ordinary and
/// release builds are immutably bound to GitHub.
#[cfg(litci_test_boundaries)]
const TEST_OAUTH_BASE_URL_ENV: &str = "LITCI_TEST_GITHUB_OAUTH_BASE_URL";

#[cfg(litci_test_boundaries)]
fn allow_keyring() -> bool {
    std::env::var_os(TEST_NO_KEYRING_ENV).is_none()
}

#[cfg(not(litci_test_boundaries))]
fn allow_keyring() -> bool {
    true
}

#[cfg(litci_test_boundaries)]
fn keyring_description() -> Result<String, String> {
    if !allow_keyring() {
        return Ok("litci-test:disabled".to_string());
    }

    let raw = std::env::var_os(TEST_KEYRING_DESCRIPTION_ENV).ok_or_else(|| {
        "the credential test boundary enabled keyring access without a unique key description"
            .to_string()
    })?;
    let value = raw.into_string().map_err(|_| {
        "the internal credential-test key description is not valid UTF-8".to_string()
    })?;
    let valid = value
        .strip_prefix("litci-test:")
        .is_some_and(|suffix| !suffix.is_empty())
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte));
    if !valid {
        return Err(
            "the internal credential-test key description is invalid; expected a unique `litci-test:` name"
                .to_string(),
        );
    }
    Ok(value)
}

#[cfg(not(litci_test_boundaries))]
fn keyring_description() -> Result<String, String> {
    Ok(token_store::DEFAULT_KEYRING_DESCRIPTION.to_string())
}

#[cfg(litci_test_boundaries)]
fn oauth_base_url() -> String {
    std::env::var(TEST_OAUTH_BASE_URL_ENV)
        .unwrap_or_else(|_| device_flow::DEFAULT_OAUTH_BASE_URL.to_string())
}

#[cfg(not(litci_test_boundaries))]
fn oauth_base_url() -> String {
    device_flow::DEFAULT_OAUTH_BASE_URL.to_string()
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
    let description = keyring_description()?;
    // `add_key` updates an existing user key by description. Do not invalidate
    // the working credential first: validation or keyring failure while
    // replacing it must leave the previous login usable.
    token_store::save(&home, stored, allow_keyring(), &description)
        .map_err(|error| format!("could not store the credential: {error}"))?;
    writeln!(out, "Stored the token in the system keyring.").ok();
    writeln!(
        out,
        "Credential use remains quarantined until stabilization Phase 16 certifies trust and input preflight."
    )
    .ok();
    Ok(())
}
