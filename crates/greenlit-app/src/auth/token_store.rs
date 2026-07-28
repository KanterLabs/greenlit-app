//! Stored-credential persistence for `litci auth`.
//!
//! Phase 12 containment forbids serializing bearer or refresh tokens to disk.
//! The Linux kernel persistent keyring is therefore the only credential
//! backend. If it is unavailable, authentication fails with an actionable
//! diagnostic rather than falling back to `~/.litci/auth.json`.
//!
//! # Keyring choice
//!
//! The kernel *persistent* keyring (`keyrings(7)`, `KEYCTL_GET_PERSISTENT`)
//! is used via the [`linux-keyutils`](https://docs.rs/linux-keyutils) crate
//! rather than the cross-platform `keyring` crate — see the dependency
//! justification comment in `Cargo.toml` and `ARCHITECTURE.md` "Known issues
//! log" for the full reasoning (no D-Bus/session-daemon dependency; v0 is
//! Linux-only so a cross-platform abstraction buys nothing). The persistent
//! keyring survives across separate `litci` process invocations for the same
//! host user (that is the entire reason it exists — plain `Session`/
//! `Process` keyrings do not), but it is kernel-resident, not disk-resident:
//! a host reboot clears it exactly like any in-memory session credential
//! cache, and the kernel expires it automatically after
//! `/proc/sys/kernel/keys/persistent_keyring_expiry` seconds of disuse
//! (refreshed on every successful access). Phase 12 intentionally provides
//! no reader for this key: Phase 16 must introduce a trust-scoped acquisition
//! path before a workflow can consume it.
//!
//! # `allow_keyring`
//!
//! Every entry point here takes an explicit `allow_keyring: bool` rather
//! than reading an environment variable itself. `crate::auth` owns the
//! custom-cfg-only `LITCI_TEST_NO_KEYRING` switch used by portable
//! compiled-CLI cases, while ordinary and release builds always enable
//! keyring access. The dedicated credential capability target instead runs
//! in an isolated Linux user and anonymous session-keyring boundary, selects
//! a unique `litci-test:` description through the same custom cfg, exercises
//! the persistent-ring path across separate processes, and unlinks that
//! exact key before teardown.

use std::fs::File;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use linux_keyutils::{KeyRing, KeyRingIdentifier};
use rustix::fs::{Mode, OFlags, openat};
use rustix::io::Errno;
use serde::Serialize;

/// Production key description inside the current user's persistent keyring.
#[cfg(not(litci_test_boundaries))]
pub(crate) const DEFAULT_KEYRING_DESCRIPTION: &str = "litci-github-token";
const AUTH_FILE_NAME: &str = "auth.json";
/// A stored-token payload is a small JSON object; this bounds a corrupted or
/// hostile keyring entry rather than reflecting an expected size. Linux
/// `user` keys accept at most 32,767 payload bytes.
const MAX_STORED_BYTES: usize = 32_767;

/// How a stored access token was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TokenSource {
    /// Obtained via the GitHub App device flow (`litci auth`).
    DeviceFlow,
    /// Pasted by the user (`litci auth --pat`).
    Pat,
    /// Read from `gh auth token` (`litci auth --gh`).
    Gh,
}

/// The complete persisted credential state for one authenticated identity.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct StoredToken {
    /// Bearer token retained for a future Phase 16 trust-scoped consumer.
    pub(crate) access_token: String,
    /// The device-flow refresh token, when one exists.
    pub(crate) refresh_token: Option<String>,
    /// Unix seconds at which `access_token` expires. `None` means the stored
    /// source did not provide a reliable expiry.
    pub(crate) access_token_expires_at: Option<u64>,
    /// Unix seconds at which `refresh_token` itself expires.
    pub(crate) refresh_token_expires_at: Option<u64>,
    /// How this token was obtained.
    pub(crate) source: TokenSource,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Computes an absolute expiry timestamp `seconds_from_now` in the future.
pub(crate) fn expires_in(seconds_from_now: u64) -> u64 {
    now_unix().saturating_add(seconds_from_now)
}

/// Persists `token` in the kernel persistent keyring.
///
/// Plaintext disk fallback is forbidden. A legacy `~/.litci/auth.json`, an
/// unavailable keyring, or an invalid credential payload fails closed with
/// the action needed to recover.
pub(crate) fn save(
    home: &Path,
    token: &StoredToken,
    allow_keyring: bool,
    description: &str,
) -> Result<(), String> {
    validate_home(home)?;
    validate_token(token)?;
    ensure_legacy_file_absent(home)?;
    let payload = serde_json::to_vec(token).map_err(|_| {
        "could not encode the credential for the kernel keyring\n  fix: retry `litci auth`; if this persists, file a Greenlit defect"
            .to_string()
    })?;
    if payload.len() > MAX_STORED_BYTES {
        return Err(format!(
            "the GitHub credential exceeds Greenlit's {MAX_STORED_BYTES}-byte keyring limit\n  fix: authenticate with a valid GitHub token, then retry"
        ));
    }
    if !allow_keyring {
        return Err(keyring_required_error());
    }
    save_to_keyring(description, &payload).map_err(|_| keyring_required_error())?;
    Ok(())
}

fn persistent_ring() -> Result<KeyRing, linux_keyutils::KeyError> {
    // Links the per-UID persistent keyring into the session keyring so it
    // stays reachable for the lifetime of this process and is discoverable
    // by name from a later, separate `litci` invocation in the same login
    // session (`keyrings(7)`, `KEYCTL_GET_PERSISTENT`).
    KeyRing::get_persistent(KeyRingIdentifier::Session)
}

fn save_to_keyring(description: &str, payload: &[u8]) -> Result<(), linux_keyutils::KeyError> {
    let ring = persistent_ring()?;
    ring.add_key(description, payload)?;
    Ok(())
}

fn validate_home(home: &Path) -> Result<(), String> {
    if home.is_absolute() {
        Ok(())
    } else {
        Err(
            "could not use the credential keyring because HOME is not absolute\n  fix: set HOME to an absolute directory, then retry `litci auth`"
                .to_string(),
        )
    }
}

fn validate_token(token: &StoredToken) -> Result<(), String> {
    if token.access_token.is_empty() || token.refresh_token.as_ref().is_some_and(String::is_empty) {
        return Err(
            "could not store an empty GitHub credential\n  fix: authenticate with a valid GitHub token, then retry"
                .to_string(),
        );
    }
    Ok(())
}

fn ensure_legacy_file_absent(home: &Path) -> Result<(), String> {
    let Some(state_dir) = open_state_dir(home)? else {
        return Ok(());
    };
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    match openat(&state_dir, AUTH_FILE_NAME, flags, Mode::empty()) {
        Err(Errno::NOENT) => Ok(()),
        Ok(_) | Err(Errno::LOOP | Errno::NOTDIR) => Err(
            "Greenlit refused the legacy plaintext credential at ~/.litci/auth.json\n  fix: remove ~/.litci/auth.json, then run `litci auth` to store the credential in the kernel keyring"
                .to_string(),
        ),
        Err(_) => Err(
            "Greenlit could not safely inspect the legacy credential path ~/.litci/auth.json\n  fix: repair permissions on ~/.litci, remove auth.json, then run `litci auth`"
                .to_string(),
        ),
    }
}

fn open_state_dir(home: &Path) -> Result<Option<File>, String> {
    let home_dir = File::open(home).map_err(|_| {
        "Greenlit could not safely inspect HOME for legacy credentials\n  fix: repair HOME permissions, then retry `litci auth`"
            .to_string()
    })?;
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW;
    match openat(&home_dir, ".litci", flags, Mode::empty()) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => Err(
            "Greenlit refused an unsafe ~/.litci path while checking legacy credentials\n  fix: replace ~/.litci with a user-owned directory, then retry `litci auth`"
                .to_string(),
        ),
        Err(_) => Err(
            "Greenlit could not safely inspect ~/.litci for legacy credentials\n  fix: repair ~/.litci permissions, then retry `litci auth`"
                .to_string(),
        ),
    }
}

fn keyring_required_error() -> String {
    "Greenlit could not store the GitHub credential because the Linux kernel persistent keyring is unavailable\n  fix: enable kernel keyring support for this user, then retry `litci auth`; plaintext credential files are disabled"
        .to_string()
}
