//! Encrypted repository-local secret persistence and legacy dotenv migration.
//!
//! Persisted values live in `.litci/secrets.vault`, authenticated with
//! AES-256-GCM using a random key stored at mode 0600 in
//! `~/.litci/vault.key`. A legacy `.litci/secrets` dotenv file is read with
//! no-follow and bounded-input semantics, encrypted atomically, and removed
//! only after the vault and its parent directory have been synchronized.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
use rustix::io::Errno;

use super::validate_name;
use crate::dotenv_format::{DotenvError, parse_dotenv};

const MAX_VAULT_BYTES: usize = 1024 * 1024;
const MAX_ASSIGNMENTS: usize = 2_000;
const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const MAGIC: &[u8] = b"greenlit-vault-v1\0";
const VAULT_NAME: &str = "secrets.vault";
const LEGACY_NAME: &str = "secrets";

fn safe(text: &str) -> String {
    crate::render::terminal::inline_escape(text)
}

fn safe_path(path: &Path) -> String {
    safe(&path.display().to_string())
}

fn vault_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".litci").join(VAULT_NAME)
}

/// Reads the encrypted repository-local vault. If only the legacy dotenv file
/// exists, migrates it to encrypted storage before returning its values.
pub(crate) fn read_dotenv_secrets(
    repo_root: &Path,
) -> Result<Option<Vec<(String, String)>>, String> {
    let Some(litci_dir) = open_repo_litci(repo_root, false)? else {
        return Ok(None);
    };
    if let Some(bytes) = read_bounded_file(&litci_dir, VAULT_NAME, &vault_path(repo_root))? {
        return decrypt_vault(&bytes).map(Some);
    }
    let legacy_path = repo_root.join(".litci").join(LEGACY_NAME);
    let Some(bytes) = read_bounded_file(&litci_dir, LEGACY_NAME, &legacy_path)? else {
        return Ok(None);
    };
    let entries = parse_legacy(&legacy_path, bytes)?;
    persist_vault(repo_root, &litci_dir, &entries)?;
    unlinkat(&litci_dir, LEGACY_NAME, AtFlags::empty()).map_err(|error| {
        format!(
            "{}: encrypted migration succeeded but the plaintext legacy file could not be removed: {error}\n  fix: remove {} manually, then retry",
            safe_path(&legacy_path),
            safe_path(&legacy_path)
        )
    })?;
    litci_dir
        .sync_all()
        .map_err(|error| vault_write_error(&vault_path(repo_root), error))?;
    Ok(Some(entries))
}

/// Adds or replaces one persisted secret in the encrypted vault.
pub(crate) fn append_secret(repo_root: &Path, name: &str, value: &str) -> Result<(), String> {
    let existing = read_dotenv_secrets(repo_root)?.unwrap_or_default();
    let mut entries = Vec::with_capacity(existing.len().saturating_add(1));
    let mut replaced = false;
    for (existing_name, existing_value) in existing {
        if existing_name.eq_ignore_ascii_case(name) {
            if !replaced {
                entries.push((name.to_string(), value.to_string()));
                replaced = true;
            }
        } else {
            entries.push((existing_name, existing_value));
        }
    }
    if !replaced {
        entries.push((name.to_string(), value.to_string()));
    }
    if entries.len() > MAX_ASSIGNMENTS {
        return Err(format!(
            "the encrypted secret vault would exceed Greenlit's {MAX_ASSIGNMENTS}-assignment safety limit\n  fix: remove an unused secret, then retry"
        ));
    }
    let litci_dir = open_repo_litci(repo_root, true)?.ok_or_else(|| {
        "could not create the repository-local .litci directory\n  fix: check repository permissions, then retry"
            .to_string()
    })?;
    persist_vault(repo_root, &litci_dir, &entries)?;
    ensure_gitignored(repo_root)
}

fn open_repo_litci(repo_root: &Path, create: bool) -> Result<Option<File>, String> {
    let repo_dir = File::open(repo_root)
        .map_err(|error| format!("could not open the repository root: {error}"))?;
    if !repo_dir
        .metadata()
        .map_err(|error| format!("could not inspect the repository root: {error}"))?
        .is_dir()
    {
        return Err(
            "repository root is not a directory\n  fix: run litci inside a Git repository"
                .to_string(),
        );
    }
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW;
    match openat(&repo_dir, ".litci", flags, Mode::empty()) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(Errno::NOENT) if !create => Ok(None),
        Err(Errno::NOENT) => {
            rustix::fs::mkdirat(
                &repo_dir,
                ".litci",
                Mode::RUSR | Mode::WUSR | Mode::XUSR,
            )
            .map_err(|error| format!("could not create .litci/: {error}"))?;
            let fd = openat(&repo_dir, ".litci", flags, Mode::empty())
                .map_err(|error| format!("could not open .litci/ after creating it: {error}"))?;
            Ok(Some(File::from(fd)))
        }
        Err(Errno::LOOP | Errno::NOTDIR) => Err(
            ".litci must be a real repository-local directory, not a symbolic link\n  fix: replace .litci with a directory, then retry"
                .to_string(),
        ),
        Err(error) => Err(format!("could not open .litci/: {error}")),
    }
}

fn read_bounded_file(
    directory: &File,
    name: &str,
    display_path: &Path,
) -> Result<Option<Vec<u8>>, String> {
    let mut file = match openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => {
            return Err(format!(
                "{} must be a regular file, not a symbolic link\n  fix: replace it with a repository-local regular file, then retry",
                safe_path(display_path)
            ));
        }
        Err(error) => {
            return Err(format!(
                "{}: could not open local secrets: {error}\n  fix: make it a readable regular file, then retry",
                safe_path(display_path)
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| read_error(display_path, error))?;
    if !metadata.is_file() {
        return Err(format!(
            "{}: local secrets path is not a regular file\n  fix: replace it with a regular file, then retry",
            safe_path(display_path)
        ));
    }
    if metadata.len() > MAX_VAULT_BYTES as u64 {
        return Err(size_limit_error(display_path));
    }
    let mut bytes = Vec::with_capacity(MAX_VAULT_BYTES.saturating_add(1));
    (&mut file)
        .take((MAX_VAULT_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| read_error(display_path, error))?;
    if bytes.len() > MAX_VAULT_BYTES {
        return Err(size_limit_error(display_path));
    }
    Ok(Some(bytes))
}

fn parse_legacy(path: &Path, bytes: Vec<u8>) -> Result<Vec<(String, String)>, String> {
    let source = String::from_utf8(bytes).map_err(|_| {
        format!(
            "{}: legacy secrets file is not valid UTF-8\n  fix: save it as UTF-8, then retry",
            safe_path(path)
        )
    })?;
    parse_dotenv(
        source.strip_prefix('\u{feff}').unwrap_or(&source),
        validate_name,
        MAX_ASSIGNMENTS,
    )
    .map_err(|error| match error {
        DotenvError::Syntax { line } => format!(
            "{}:{line}: could not parse legacy secrets as KEY=VALUE\n  fix: correct the dotenv syntax near line {line}, then retry",
            safe_path(path)
        ),
        DotenvError::InvalidName {
            line,
            name,
            reason,
        } => format!(
            "{}:{line}: invalid secret name '{}': {}\n  fix: use only letters, digits, and underscores, starting with a letter or underscore and not GITHUB_",
            safe_path(path),
            safe(&name),
            safe(reason)
        ),
        DotenvError::AssignmentLimit => format!(
            "{}: legacy secrets exceed Greenlit's {MAX_ASSIGNMENTS}-assignment safety limit\n  fix: reduce the file, then retry",
            safe_path(path)
        ),
    })
}

fn decrypt_vault(bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let path = Path::new(".litci/secrets.vault");
    let header = MAGIC.len().saturating_add(NONCE_BYTES);
    if bytes.len() < header.saturating_add(TAG_BYTES) || !bytes.starts_with(MAGIC) {
        return Err(vault_format_error(path));
    }
    let nonce_bytes: [u8; NONCE_BYTES] = bytes[MAGIC.len()..header]
        .try_into()
        .map_err(|_| vault_format_error(path))?;
    let key = load_vault_key(false)?.ok_or_else(|| {
        "the encrypted secret vault exists but ~/.litci/vault.key is missing\n  fix: restore the original key or remove .litci/secrets.vault and add the secrets again"
            .to_string()
    })?;
    let cipher = cipher(&key)?;
    let mut plaintext = bytes[header..].to_vec();
    let opened = cipher
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(MAGIC),
            &mut plaintext,
        )
        .map_err(|_| {
            "could not authenticate the encrypted secret vault\n  fix: restore the matching ~/.litci/vault.key or recreate the vault"
                .to_string()
        })?;
    let entries: Vec<(String, String)> = serde_json::from_slice(opened).map_err(|_| {
        "the authenticated secret vault has an invalid payload\n  fix: remove the vault and add the secrets again"
            .to_string()
    })?;
    validate_entries(&entries)?;
    Ok(entries)
}

fn persist_vault(
    repo_root: &Path,
    litci_dir: &File,
    entries: &[(String, String)],
) -> Result<(), String> {
    validate_entries(entries)?;
    let plaintext = serde_json::to_vec(entries)
        .map_err(|error| format!("could not encode encrypted secrets: {error}"))?;
    let key = load_vault_key(true)?.ok_or_else(|| {
        "could not create the secret-vault encryption key\n  fix: make ~/.litci writable, then retry"
            .to_string()
    })?;
    let mut nonce_bytes = [0_u8; NONCE_BYTES];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| "could not obtain secure randomness for secret encryption".to_string())?;
    let mut ciphertext = plaintext;
    cipher(&key)?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(MAGIC),
            &mut ciphertext,
        )
        .map_err(|_| "could not encrypt the local secret vault".to_string())?;
    let total = MAGIC
        .len()
        .saturating_add(NONCE_BYTES)
        .saturating_add(ciphertext.len());
    if total > MAX_VAULT_BYTES {
        return Err(
            "the encrypted secret vault would exceed Greenlit's 1 MiB safety limit\n  fix: remove or shorten persisted secrets, then retry"
                .to_string(),
        );
    }
    let mut payload = Vec::with_capacity(total);
    payload.extend_from_slice(MAGIC);
    payload.extend_from_slice(&nonce_bytes);
    payload.extend_from_slice(&ciphertext);
    write_atomic_at(litci_dir, VAULT_NAME, &payload, &vault_path(repo_root))
}

fn cipher(key: &[u8; KEY_BYTES]) -> Result<LessSafeKey, String> {
    UnboundKey::new(&AES_256_GCM, key)
        .map(LessSafeKey::new)
        .map_err(|_| "could not initialize secret-vault encryption".to_string())
}

fn load_vault_key(create: bool) -> Result<Option<[u8; KEY_BYTES]>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            "HOME must be set to an absolute path to use encrypted secrets\n  fix: set HOME, then retry"
                .to_string()
        })?;
    let home_dir = File::open(&home)
        .map_err(|error| format!("could not open HOME for the secret vault: {error}"))?;
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW;
    let state_dir = match openat(&home_dir, ".litci", flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) if !create => return Ok(None),
        Err(Errno::NOENT) => {
            rustix::fs::mkdirat(&home_dir, ".litci", Mode::RUSR | Mode::WUSR | Mode::XUSR)
                .map_err(|error| format!("could not create ~/.litci: {error}"))?;
            File::from(
                openat(&home_dir, ".litci", flags, Mode::empty())
                    .map_err(|error| format!("could not open ~/.litci: {error}"))?,
            )
        }
        Err(error) => return Err(format!("could not open ~/.litci: {error}")),
    };
    match read_key(&state_dir)? {
        Some(key) => Ok(Some(key)),
        None if !create => Ok(None),
        None => {
            let mut key = [0_u8; KEY_BYTES];
            SystemRandom::new()
                .fill(&mut key)
                .map_err(|_| "could not obtain secure randomness for the vault key".to_string())?;
            write_atomic_at(
                &state_dir,
                "vault.key",
                &key,
                &home.join(".litci/vault.key"),
            )?;
            Ok(Some(key))
        }
    }
}

fn read_key(state_dir: &File) -> Result<Option<[u8; KEY_BYTES]>, String> {
    let mut file = match openat(
        state_dir,
        "vault.key",
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(format!("could not open ~/.litci/vault.key: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect ~/.litci/vault.key: {error}"))?;
    if !metadata.is_file() || metadata.len() != KEY_BYTES as u64 {
        return Err(
            "~/.litci/vault.key is not a valid 32-byte regular file\n  fix: restore the original key or recreate the secret vault"
                .to_string(),
        );
    }
    let mut key = [0_u8; KEY_BYTES];
    file.read_exact(&mut key)
        .map_err(|error| format!("could not read ~/.litci/vault.key: {error}"))?;
    Ok(Some(key))
}

fn write_atomic_at(
    directory: &File,
    name: &str,
    bytes: &[u8],
    display_path: &Path,
) -> Result<(), String> {
    let mut suffix = [0_u8; 8];
    SystemRandom::new()
        .fill(&mut suffix)
        .map_err(|_| "could not obtain secure randomness for an atomic write".to_string())?;
    let temp_name = format!(
        ".{name}.tmp-{}",
        suffix
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let mode = Mode::RUSR | Mode::WUSR;
    let fd = openat(
        directory,
        temp_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        mode,
    )
    .map_err(|error| vault_write_error(display_path, error))?;
    let mut file = File::from(fd);
    let write_result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| vault_write_error(display_path, error));
    if let Err(error) = write_result {
        let _ = unlinkat(directory, temp_name.as_str(), AtFlags::empty());
        return Err(error);
    }
    renameat(directory, temp_name.as_str(), directory, name)
        .map_err(|error| vault_write_error(display_path, error))?;
    directory
        .sync_all()
        .map_err(|error| vault_write_error(display_path, error))
}

fn validate_entries(entries: &[(String, String)]) -> Result<(), String> {
    if entries.len() > MAX_ASSIGNMENTS {
        return Err(
            "the authenticated secret vault exceeds Greenlit's 2,000-assignment safety limit\n  fix: recreate the vault with fewer entries"
                .to_string(),
        );
    }
    for (name, _) in entries {
        validate_name(name).map_err(|reason| {
            format!(
                "the authenticated secret vault contains invalid name '{}': {}\n  fix: recreate the vault with valid secret names",
                safe(name),
                safe(reason)
            )
        })?;
    }
    Ok(())
}

fn ensure_gitignored(repo_root: &Path) -> Result<(), String> {
    const LINE: &str = ".litci/\n";
    let path = repo_root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("could not read .gitignore: {error}")),
    };
    if existing
        .lines()
        .any(|line| line.trim() == ".litci/" || line.trim() == ".litci")
    {
        return Ok(());
    }
    let mut options = std::fs::OpenOptions::new();
    let mut file = options
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("could not open .gitignore for writing: {error}"))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")
            .map_err(|error| format!("could not append to .gitignore: {error}"))?;
    }
    file.write_all(LINE.as_bytes())
        .map_err(|error| format!("could not append to .gitignore: {error}"))
}

fn read_error(path: &Path, error: std::io::Error) -> String {
    format!(
        "{}: could not read local secrets: {}\n  fix: make it a readable regular file, then retry",
        safe_path(path),
        safe(&error.to_string())
    )
}

fn size_limit_error(path: &Path) -> String {
    format!(
        "{}: local secrets exceed Greenlit's 1 MiB safety limit\n  fix: reduce the file, then retry",
        safe_path(path)
    )
}

fn vault_write_error(path: &Path, error: impl std::fmt::Display) -> String {
    format!(
        "could not persist encrypted secrets at {}: {error}\n  fix: ensure the filesystem has free space and is writable, then retry",
        safe_path(path)
    )
}

fn vault_format_error(path: &Path) -> String {
    format!(
        "{} is not a Greenlit encrypted secret vault\n  fix: restore a valid vault or remove it and add the secrets again",
        safe_path(path)
    )
}
