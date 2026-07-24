//! Stored-credential persistence for `litci auth`: the Linux kernel keyring
//! first, falling back to a `0600` file under `~/.litci/` with a printed
//! warning (`PHASE-3-actions.md` Auth: "store token + refresh token in the
//! system keyring, fall back to a 0600 file under `~/.litci/` with a printed
//! warning").
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
//! (refreshed on every successful access). Either case surfaces to the user
//! as an ordinary "not authenticated" state — [`load`] returning `None` —
//! which every caller already handles by pointing at `litci auth`, so no
//! special-case recovery is needed.
//!
//! # `allow_keyring`
//!
//! Every entry point here takes an explicit `allow_keyring: bool` rather
//! than reading an environment variable itself. Production call sites
//! (`crate::auth`) always pass `true`; the compiled-binary integration tests
//! (`crates/greenlit-app/tests/`) run through the real `litci` binary and so
//! cannot inject a Rust-level `bool` — they instead set the internal
//! `LITCI_TEST_NO_KEYRING` process-environment variable, which `main.rs`
//! reads once at startup and threads down as this same `bool`. This module
//! itself never touches that variable, so its own unit tests can force the
//! file-only path with a plain function argument — no process-global
//! `std::env::set_var` (which `#![forbid(unsafe_code)]` disallows in this
//! crate as of Rust edition 2024) is needed anywhere. The reason to force
//! file-only at all: the kernel keyring is scoped to the calling process's
//! UID, not to `$HOME`, so it is not sandboxable the way the file fallback
//! is — an integration test that let it run for real would read/write the
//! *real* test-runner account's persistent keyring. The keyring code path
//! itself is instead covered by a unit test below scoped to the `Thread`
//! keyring identifier, which never touches persistent kernel state.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{Mode, OFlags, openat};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use linux_keyutils::{KeyRing, KeyRingIdentifier};

const KEYRING_DESCRIPTION: &str = "litci-github-token";
const AUTH_FILE_NAME: &str = "auth.json";
/// A stored-token payload is a small JSON object; this bounds a corrupted or
/// hostile file/keyring entry rather than reflecting an expected size.
const MAX_STORED_BYTES: usize = 64 * 1024;

/// How a stored access token was obtained — governs whether a refresh
/// attempt applies (device-flow only; PAT/`gh` tokens carry no refresh
/// token).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredToken {
    /// The bearer token used for API requests.
    pub(crate) access_token: String,
    /// The device-flow refresh token, when one exists.
    pub(crate) refresh_token: Option<String>,
    /// Unix seconds at which `access_token` expires. `None` means litci has
    /// no reliable expiry to check (a pasted PAT or a `gh`-sourced token) —
    /// callers then rely on the API itself reporting an auth failure.
    pub(crate) access_token_expires_at: Option<u64>,
    /// Unix seconds at which `refresh_token` itself expires.
    pub(crate) refresh_token_expires_at: Option<u64>,
    /// How this token was obtained.
    pub(crate) source: TokenSource,
}

/// Which backend a save actually used, so the caller can print the
/// fallback warning `PHASE-3-actions.md` requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreBackend {
    /// The kernel persistent keyring.
    Keyring,
    /// The `0600` file fallback.
    File,
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

/// Whether `expires_at` (a stored expiry) has already passed, with a small
/// safety margin so a token is refreshed slightly before the exact instant
/// it stops working rather than racing it.
pub(crate) fn is_expired(expires_at: u64) -> bool {
    const SAFETY_MARGIN_SECS: u64 = 60;
    now_unix().saturating_add(SAFETY_MARGIN_SECS) >= expires_at
}

/// Persists `token`, trying the kernel keyring first (when `allow_keyring`)
/// and falling back to the `0600` file under `home/.litci/auth.json` on any
/// keyring failure (disabled, missing kernel support, quota, etc.).
pub(crate) fn save(
    home: &Path,
    token: &StoredToken,
    allow_keyring: bool,
) -> Result<StoreBackend, String> {
    let payload = serde_json::to_vec(token)
        .map_err(|error| format!("could not serialize the credential payload: {error}"))?;
    if allow_keyring && save_to_keyring(&payload).is_ok() {
        return Ok(StoreBackend::Keyring);
    }
    save_to_file(home, &payload)?;
    Ok(StoreBackend::File)
}

/// Loads the currently stored token, trying the keyring (when
/// `allow_keyring`) then the file. Returns `None` when neither backend
/// holds a (valid) entry — the caller treats this identically to "never
/// authenticated".
pub(crate) fn load(home: &Path, allow_keyring: bool) -> Option<StoredToken> {
    if allow_keyring && let Some(token) = load_from_keyring() {
        return Some(token);
    }
    load_from_file(home)
}

/// Removes any stored token from every backend this module writes to,
/// leaving the caller effectively unauthenticated. Used by `litci auth` to
/// replace a stale credential outright rather than potentially leaving both
/// an old keyring entry and a new file (or vice versa) simultaneously
/// resolvable.
pub(crate) fn clear(home: &Path, allow_keyring: bool) {
    if allow_keyring
        && let Ok(ring) = persistent_ring()
        && let Ok(key) = ring.search(KEYRING_DESCRIPTION)
    {
        let _ = key.invalidate();
    }
    let _ = std::fs::remove_file(auth_file_path(home));
}

fn persistent_ring() -> Result<KeyRing, linux_keyutils::KeyError> {
    // Links the per-UID persistent keyring into the session keyring so it
    // stays reachable for the lifetime of this process and is discoverable
    // by name from a later, separate `litci` invocation in the same login
    // session (`keyrings(7)`, `KEYCTL_GET_PERSISTENT`).
    KeyRing::get_persistent(KeyRingIdentifier::Session)
}

fn save_to_keyring(payload: &[u8]) -> Result<(), linux_keyutils::KeyError> {
    let ring = persistent_ring()?;
    ring.add_key(KEYRING_DESCRIPTION, payload)?;
    Ok(())
}

fn load_from_keyring() -> Option<StoredToken> {
    let ring = persistent_ring().ok()?;
    let key = ring.search(KEYRING_DESCRIPTION).ok()?;
    let bytes = key.read_to_vec().ok()?;
    if bytes.len() > MAX_STORED_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn auth_file_path(home: &Path) -> PathBuf {
    home.join(".litci").join(AUTH_FILE_NAME)
}

fn save_to_file(home: &Path, payload: &[u8]) -> Result<(), String> {
    let home_dir = std::fs::File::open(home)
        .map_err(|error| format!("could not open home directory {}: {error}", home.display()))?;
    let litci_dir = match openat(
        &home_dir,
        ".litci",
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
        Mode::empty(),
    ) {
        Ok(fd) => std::fs::File::from(fd),
        Err(Errno::NOENT) => {
            rustix::fs::mkdirat(&home_dir, ".litci", Mode::RUSR | Mode::WUSR | Mode::XUSR)
                .map_err(|error| format!("could not create ~/.litci: {error}"))?;
            let fd = openat(
                &home_dir,
                ".litci",
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
                Mode::empty(),
            )
            .map_err(|error| format!("could not open ~/.litci after creating it: {error}"))?;
            std::fs::File::from(fd)
        }
        Err(error) => return Err(format!("could not open ~/.litci: {error}")),
    };
    let mode = Mode::RUSR | Mode::WUSR;
    let fd = openat(
        &litci_dir,
        AUTH_FILE_NAME,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        mode,
    )
    .map_err(|error| format!("could not open ~/.litci/{AUTH_FILE_NAME}: {error}"))?;
    let mut file = std::fs::File::from(fd);
    // `O_CREAT` alone does not tighten an already-existing file's mode, so
    // fix it explicitly every save (belt-and-suspenders: `$HOME` is a
    // trusted, single-user directory, but the credential file itself must
    // never be group/other-readable regardless of prior contents).
    rustix::fs::fchmod(&file, mode).ok();
    use std::io::Write;
    file.write_all(payload)
        .map_err(|error| format!("could not write ~/.litci/{AUTH_FILE_NAME}: {error}"))
}

fn load_from_file(home: &Path) -> Option<StoredToken> {
    let home_dir = std::fs::File::open(home).ok()?;
    let litci_dir = openat(
        &home_dir,
        ".litci",
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .ok()?;
    let litci_dir = std::fs::File::from(litci_dir);
    let fd = openat(
        &litci_dir,
        AUTH_FILE_NAME,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .ok()?;
    let mut file = std::fs::File::from(fd);
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_STORED_BYTES as u64 {
        return None;
    }
    use std::io::Read;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(source: TokenSource) -> StoredToken {
        StoredToken {
            access_token: "ghu_example".to_string(),
            refresh_token: Some("ghr_example".to_string()),
            access_token_expires_at: Some(expires_in(28_800)),
            refresh_token_expires_at: Some(expires_in(15_897_600)),
            source,
        }
    }

    #[test]
    fn file_backend_round_trips_and_is_mode_0600() {
        let home = tempfile::tempdir().expect("tempdir");
        let token = sample(TokenSource::Pat);
        let backend = save(home.path(), &token, false).expect("save");
        assert_eq!(backend, StoreBackend::File);

        let loaded = load(home.path(), false).expect("load");
        assert_eq!(loaded, token);

        let metadata = std::fs::metadata(home.path().join(".litci").join(AUTH_FILE_NAME))
            .expect("stat auth file");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn missing_store_loads_as_none() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(load(home.path(), false).is_none());
    }

    #[test]
    fn clear_removes_the_file_backend() {
        let home = tempfile::tempdir().expect("tempdir");
        save(home.path(), &sample(TokenSource::Gh), false).expect("save");
        clear(home.path(), false);
        assert!(load(home.path(), false).is_none());
    }

    #[test]
    fn expiry_math_has_a_safety_margin() {
        assert!(is_expired(now_unix()));
        assert!(is_expired(now_unix() + 30));
        assert!(!is_expired(now_unix() + 3600));
    }

    /// Exercises the real kernel-keyring syscalls this module drives
    /// (`TESTING.md`: "Mock only true externals"; the kernel keyring is one),
    /// scoped to the calling *thread's* private keyring rather than the
    /// per-UID persistent one `save`/`load` use in production — `Thread` is
    /// never linked into any session and is destroyed with the thread, so
    /// this can never leak into or collide with a real `litci auth` session
    /// on the machine running the test.
    #[test]
    fn kernel_keyring_add_and_search_round_trip_on_a_thread_scoped_ring() {
        let ring = match KeyRing::from_special_id(KeyRingIdentifier::Thread, true) {
            Ok(ring) => ring,
            // A kernel without keyring support (or a sandboxed CI runner
            // that denies the syscall) cannot exercise this boundary; the
            // production fallback for exactly this case is the file store,
            // covered above.
            Err(_) => return,
        };
        let payload = b"thread-scoped-roundtrip-probe";
        ring.add_key("litci-test-probe", payload)
            .expect("add_key on thread ring");
        let key = ring.search("litci-test-probe").expect("search thread ring");
        let read_back = key.read_to_vec().expect("read_to_vec");
        assert_eq!(read_back, payload);
    }
}
