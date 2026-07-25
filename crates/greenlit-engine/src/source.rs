//! Race-checked, Git-aware source snapshot capture.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_PATHS: usize = 1_000_000;
const MAX_PATH_BYTES: usize = 64 * 1024;
const CAPTURE_ATTEMPTS: usize = 3;

/// Type of one canonical source-tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceEntryKind {
    /// Ordinary file.
    File,
    /// Symbolic link; the digest covers its target bytes.
    Symlink,
}

/// One entry in a canonical source-tree manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    /// Repository-relative slash-separated path.
    pub path: String,
    /// Entry kind.
    pub kind: SourceEntryKind,
    /// Git-compatible mode (`100644`, `100755`, or `120000`).
    pub mode: u32,
    /// SHA-256 digest of file bytes or symlink target bytes.
    pub digest: String,
    /// File or target byte length.
    pub size: u64,
}

/// A verified frozen checkout and its canonical identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSnapshot {
    /// Full commit at capture time.
    pub commit: String,
    /// Whether captured current bytes differ from `HEAD`.
    pub dirty: bool,
    /// Digest of canonical manifest JSON.
    pub digest: String,
    /// Canonically ordered source entries.
    pub entries: Vec<SourceEntry>,
    /// Materialized self-contained checkout.
    pub root: PathBuf,
}

/// Failure to capture an immutable current-worktree snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SourceSnapshotError {
    /// A local Git command could not provide required data.
    #[error("could not freeze repository source with 'git {args}': {message}")]
    Git {
        /// Arguments passed after `git -C <repo>`.
        args: String,
        /// Bounded diagnostic.
        message: String,
    },
    /// Filesystem capture failed.
    #[error("could not freeze source path {path}: {message}")]
    Io {
        /// Affected path.
        path: String,
        /// I/O diagnostic.
        message: String,
    },
    /// Repository contains an entry v0 cannot safely represent.
    #[error(
        "source path {path} has unsupported type; only regular files and symbolic links can be frozen"
    )]
    UnsupportedType {
        /// Repository-relative path.
        path: String,
    },
    /// Source did not remain stable long enough to lock.
    #[error("repository source changed repeatedly while Greenlit was freezing it")]
    ChangedDuringCapture,
    /// A path cannot be represented in the canonical JSON schema.
    #[error("source path is not valid UTF-8 and cannot be represented canonically")]
    NonUtf8Path,
    /// Safety limit exceeded.
    #[error("repository source exceeds the safety limit of {limit} paths")]
    PathLimit {
        /// Maximum accepted path count.
        limit: usize,
    },
}

impl SourceSnapshot {
    /// Captures current tracked bytes plus untracked nonignored files into
    /// `destination`. The destination must not already exist.
    pub fn capture(repo_root: &Path, destination: &Path) -> Result<Self, SourceSnapshotError> {
        for attempt in 0..CAPTURE_ATTEMPTS {
            let temp =
                destination.with_extension(format!("capture-{}-{attempt}", std::process::id()));
            remove_exact_tree_if_present(&temp)?;
            match capture_once(repo_root, &temp) {
                Ok(snapshot) => {
                    if destination.exists() {
                        remove_exact_tree_if_present(&temp)?;
                        return Err(io_error(
                            destination,
                            "destination already exists; choose a new run directory",
                        ));
                    }
                    fs::rename(&temp, destination).map_err(|error| io_error(destination, error))?;
                    return Ok(Self {
                        root: destination.to_path_buf(),
                        ..snapshot
                    });
                }
                Err(SourceSnapshotError::ChangedDuringCapture) => {
                    remove_exact_tree_if_present(&temp)?;
                }
                Err(error) => {
                    remove_exact_tree_if_present(&temp)?;
                    return Err(error);
                }
            }
        }
        Err(SourceSnapshotError::ChangedDuringCapture)
    }
}

fn capture_once(
    repo_root: &Path,
    destination: &Path,
) -> Result<SourceSnapshot, SourceSnapshotError> {
    let commit_before = git_text(repo_root, &["rev-parse", "HEAD"])?;
    let paths = git_paths(
        repo_root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;

    clone_git_metadata(repo_root, destination)?;
    let entries = copy_and_hash(repo_root, destination, &paths)?;
    let verified = hash_live_entries(repo_root, &paths)?;
    let commit_after = git_text(repo_root, &["rev-parse", "HEAD"])?;
    if entries != verified || commit_before != commit_after {
        return Err(SourceSnapshotError::ChangedDuringCapture);
    }
    let dirty = !git_status(repo_root)?.is_empty();
    let manifest = serde_json::to_vec(&entries).map_err(|error| SourceSnapshotError::Io {
        path: destination.display().to_string(),
        message: format!("could not serialize source manifest: {error}"),
    })?;
    let digest = sha256_identity(&manifest);
    let manifest_path = destination.join(".litci-source-manifest.json");
    let mut output = create_new_file(&manifest_path)?;
    output
        .write_all(&manifest)
        .map_err(|error| io_error(&manifest_path, error))?;
    Ok(SourceSnapshot {
        commit: commit_before,
        dirty,
        digest,
        entries,
        root: destination.to_path_buf(),
    })
}

fn clone_git_metadata(repo_root: &Path, destination: &Path) -> Result<(), SourceSnapshotError> {
    let original_origin = git_optional_text(repo_root, &["config", "--get", "remote.origin.url"])?;
    let output = Command::new("git")
        .args([
            "clone",
            "--no-hardlinks",
            "--no-checkout",
            "--no-tags",
            "--quiet",
        ])
        .arg(repo_root)
        .arg(destination)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| git_error(&["clone"], error.to_string()))?;
    if output.status.success() {
        if let Some(origin) = original_origin {
            git_output(destination, &["remote", "set-url", "origin", &origin])?;
        }
        Ok(())
    } else {
        Err(git_error(
            &["clone"],
            bounded_stderr(&output.stderr, output.status.to_string()),
        ))
    }
}

fn copy_and_hash(
    repo_root: &Path,
    destination: &Path,
    paths: &[String],
) -> Result<Vec<SourceEntry>, SourceSnapshotError> {
    let mut entries = Vec::with_capacity(paths.len());
    for relative in paths {
        let source = repo_root.join(relative);
        let target = destination.join(relative);
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(&source, error)),
        };
        if metadata.file_type().is_symlink() {
            let link_target = fs::read_link(&source).map_err(|error| io_error(&source, error))?;
            let bytes = link_target.as_os_str().as_encoded_bytes();
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
            }
            symlink(&link_target, &target).map_err(|error| io_error(&target, error))?;
            entries.push(entry(relative, SourceEntryKind::Symlink, 0o120000, bytes));
        } else if metadata.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
            }
            let mut input = File::open(&source).map_err(|error| io_error(&source, error))?;
            let mut output = create_new_file(&target)?;
            let (digest, size) = copy_hash(&mut input, &mut output, &source)?;
            let executable = metadata.mode() & 0o111 != 0;
            let mode = if executable { 0o100755 } else { 0o100644 };
            fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o777))
                .map_err(|error| io_error(&target, error))?;
            entries.push(SourceEntry {
                path: relative.clone(),
                kind: SourceEntryKind::File,
                mode,
                digest,
                size,
            });
        } else {
            return Err(SourceSnapshotError::UnsupportedType {
                path: relative.clone(),
            });
        }
    }
    Ok(entries)
}

fn hash_live_entries(
    repo_root: &Path,
    paths: &[String],
) -> Result<Vec<SourceEntry>, SourceSnapshotError> {
    let mut entries = Vec::with_capacity(paths.len());
    for relative in paths {
        let source = repo_root.join(relative);
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(&source, error)),
        };
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&source).map_err(|error| io_error(&source, error))?;
            entries.push(entry(
                relative,
                SourceEntryKind::Symlink,
                0o120000,
                target.as_os_str().as_encoded_bytes(),
            ));
        } else if metadata.is_file() {
            let mut input = File::open(&source).map_err(|error| io_error(&source, error))?;
            let (digest, size) = hash_reader(&mut input, &source)?;
            entries.push(SourceEntry {
                path: relative.clone(),
                kind: SourceEntryKind::File,
                mode: if metadata.mode() & 0o111 != 0 {
                    0o100755
                } else {
                    0o100644
                },
                digest,
                size,
            });
        } else {
            return Err(SourceSnapshotError::UnsupportedType {
                path: relative.clone(),
            });
        }
    }
    Ok(entries)
}

fn entry(relative: &str, kind: SourceEntryKind, mode: u32, bytes: &[u8]) -> SourceEntry {
    SourceEntry {
        path: relative.to_string(),
        kind,
        mode,
        digest: sha256_identity(bytes),
        size: bytes.len() as u64,
    }
}

fn copy_hash(
    input: &mut File,
    output: &mut File,
    source: &Path,
) -> Result<(String, u64), SourceSnapshotError> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| io_error(source, error))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| io_error(source, error))?;
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    Ok((format!("sha256:{}", hex_digest(&hasher.finalize())), size))
}

fn hash_reader(input: &mut File, source: &Path) -> Result<(String, u64), SourceSnapshotError> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| io_error(source, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    Ok((format!("sha256:{}", hex_digest(&hasher.finalize())), size))
}

fn git_text(repo_root: &Path, args: &[&str]) -> Result<String, SourceSnapshotError> {
    let output = git_output(repo_root, args)?;
    String::from_utf8(output)
        .map(|value| value.trim().to_string())
        .map_err(|error| git_error(args, format!("stdout was not UTF-8: {error}")))
}

fn git_optional_text(
    repo_root: &Path,
    args: &[&str],
) -> Result<Option<String>, SourceSnapshotError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| git_error(args, error.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }
    String::from_utf8(output.stdout)
        .map(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .map_err(|error| git_error(args, format!("stdout was not UTF-8: {error}")))
}

fn git_status(repo_root: &Path) -> Result<Vec<u8>, SourceSnapshotError> {
    git_output(
        repo_root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
}

fn git_paths(repo_root: &Path, args: &[&str]) -> Result<Vec<String>, SourceSnapshotError> {
    let bytes = git_output(repo_root, args)?;
    let mut paths = Vec::new();
    for raw in bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        if paths.len() == MAX_PATHS {
            return Err(SourceSnapshotError::PathLimit { limit: MAX_PATHS });
        }
        if raw.len() > MAX_PATH_BYTES {
            return Err(SourceSnapshotError::Io {
                path: repo_root.display().to_string(),
                message: format!("one source path exceeds {MAX_PATH_BYTES} bytes"),
            });
        }
        let path = std::str::from_utf8(raw).map_err(|_| SourceSnapshotError::NonUtf8Path)?;
        if path == ".litci" || path.starts_with(".litci/") {
            continue;
        }
        paths.push(path.to_string());
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn git_output(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>, SourceSnapshotError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| git_error(args, error.to_string()))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(git_error(
            args,
            bounded_stderr(&output.stderr, output.status.to_string()),
        ))
    }
}

fn create_new_file(path: &Path) -> Result<File, SourceSnapshotError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_error(path, error))
}

fn remove_exact_tree_if_present(path: &Path) -> Result<(), SourceSnapshotError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
    }
}

fn bounded_stderr(stderr: &[u8], fallback: String) -> String {
    let retained = &stderr[..stderr.len().min(64 * 1024)];
    let text = String::from_utf8_lossy(retained).trim().to_string();
    if text.is_empty() { fallback } else { text }
}

fn git_error(args: &[&str], message: String) -> SourceSnapshotError {
    SourceSnapshotError::Git {
        args: args.join(" "),
        message,
    }
}

fn io_error(path: &Path, error: impl std::fmt::Display) -> SourceSnapshotError {
    SourceSnapshotError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

fn sha256_identity(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_digest(&Sha256::digest(bytes)))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}
