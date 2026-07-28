//! Race-checked, Git-aware source snapshot capture.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod git;
mod remote;

use git::{clone_git_metadata, git_paths, git_status, git_text};

const CAPTURE_ATTEMPTS: usize = 3;
const MAX_PATH_BYTES: usize = 64 * 1024;

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
    /// The configured origin cannot be retained without risking credential
    /// persistence or changing an ambiguous transport into another identity.
    #[error(
        "could not freeze repository source because remote.origin.url is credential-bearing or uses an ambiguous transport"
    )]
    UnsafeRemote,
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

    /// Re-verifies a previously prepared snapshot against the repository's
    /// current bytes and atomically adopts it at `destination` when identical.
    ///
    /// This is the daemon fast path: prepared content is only a performance
    /// hint. The same commit, canonical entry manifest, and dirty state are
    /// recomputed before the snapshot becomes a run input. A mismatch returns
    /// [`SourceSnapshotError::ChangedDuringCapture`] so callers can discard the
    /// hint and use [`Self::capture`].
    pub fn verify_and_adopt(
        self,
        repo_root: &Path,
        destination: &Path,
    ) -> Result<Self, SourceSnapshotError> {
        if destination.exists() {
            return Err(io_error(
                destination,
                "destination already exists; choose a new run directory",
            ));
        }
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
        let entries = hash_live_entries(repo_root, &paths)?;
        let commit_after = git_text(repo_root, &["rev-parse", "HEAD"])?;
        let dirty = !git_status(repo_root)?.is_empty();
        if commit_before != commit_after
            || self.commit != commit_after
            || self.entries != entries
            || self.dirty != dirty
        {
            return Err(SourceSnapshotError::ChangedDuringCapture);
        }
        fs::rename(&self.root, destination).map_err(|error| io_error(destination, error))?;
        Ok(Self {
            root: destination.to_path_buf(),
            ..self
        })
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
    Ok(SourceSnapshot {
        commit: commit_before,
        dirty,
        digest,
        entries,
        root: destination.to_path_buf(),
    })
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
                create_private_dir_all(parent)?;
            }
            symlink(&link_target, &target).map_err(|error| io_error(&target, error))?;
            entries.push(entry(relative, SourceEntryKind::Symlink, 0o120000, bytes));
        } else if metadata.is_file() {
            if let Some(parent) = target.parent() {
                create_private_dir_all(parent)?;
            }
            let mut input = File::open(&source).map_err(|error| io_error(&source, error))?;
            let mut output = create_private_file(&target)?;
            let (digest, size) = copy_hash(&mut input, &mut output, &source)?;
            let manifest_mode = if metadata.mode() & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            };
            entries.push(SourceEntry {
                path: relative.clone(),
                kind: SourceEntryKind::File,
                mode: manifest_mode,
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

fn create_private_dir_all(path: &Path) -> Result<(), SourceSnapshotError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path).map_err(|error| io_error(path, error))
}

fn create_private_file(path: &Path) -> Result<File, SourceSnapshotError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
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
