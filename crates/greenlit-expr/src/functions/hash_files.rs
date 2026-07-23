//! `hashFiles(pattern…)`: glob matching plus GitHub's documented two-level
//! SHA-256 algorithm.
//!
//! Sources: the Actions runner's
//! [`HashFilesFunction.cs`](https://github.com/actions/runner/blob/main/src/Runner.Worker/Expressions/HashFilesFunction.cs),
//! `src/Misc/expressionFunc/hashFiles/src/hashFiles.ts`, and
//! `@actions/toolkit` `packages/glob/src/internal-*.ts`. Filesystem access is
//! behind the [`HashFilesFs`] trait specifically so tests can supply an
//! in-memory fake while [`RealFs`] ships the real descriptor-relative Linux
//! implementation now (per the Phase 1 task list).
//!
//! Match selection and traversal order follow current toolkit source.
//! Leading-`/` patterns remain filesystem-absolute. This conflicts with the
//! public docs' `/src/*.js` wording but matches runner 2.336.0 observation:
//! <https://github.com/ShaneKanterman04/greenlit-app/actions/runs/29880046283>.
//! Greenlit still skips absolute roots outside the provided workspace.
//!
//! [`RealFs`] deliberately strengthens the runner script's lexical workspace
//! check: every access re-resolves fresh from one pinned workspace root
//! descriptor, hop by hop with `openat2`, so a concurrent rename anywhere
//! along the path breaks that access (`ENOENT`) rather than silently keeping
//! it beneath the original root's now-relocated content; a file link or
//! followed directory link cannot read host files outside it either. The
//! runner at the pinned Phase 1 source revision follows such links after
//! checking only the link path. Greenlit's product-root security invariant is
//! authoritative for this boundary.
//! <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Misc/expressionFunc/hashFiles/src/hashFiles.ts>
//!
//! The runner executes hashing in a helper process and kills it after 120
//! seconds. Greenlit runs the complete filesystem computation on one bounded
//! worker and supervises that worker against the same fixed deadline. This
//! lets evaluation return on time even when a host filesystem call cannot be
//! interrupted; the detached worker retains cooperative checks and stops as
//! soon as the blocking call returns. Because a worker stuck in an
//! uninterruptible host syscall can never actually be killed from a library
//! (only a process supervisor can do that — out of scope here), starting a
//! worker reserves one slot in a fixed live-worker cap up front, so the
//! number of simultaneously live workers is itself bounded; see the internal
//! `stranded` module.

mod deadline;
mod filesystem;
mod patterns;
mod stranded;
mod traversal;

#[cfg(test)]
pub(crate) mod test_support;

use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use self::deadline::HashFilesDeadline;
use self::patterns::{compile_patterns, merged_search_roots};
use self::stranded::StrandGuard;
use self::traversal::{DirectoryRegistry, HashSource, walk_search_root};

pub use self::deadline::HashFilesClock;
pub(crate) use self::deadline::SystemHashFilesClock;
pub use self::filesystem::{DirEntry, EntryKind, HashFilesFs, OpenDirectory, OpenedDir, RealFs};

/// A `hashFiles()` failure.
#[derive(Debug, thiserror::Error)]
pub enum HashFilesError {
    /// The first argument started with `--` but wasn't
    /// `--follow-symbolic-links`.
    #[error("invalid glob option {0:?}")]
    InvalidOption(String),
    /// A pattern contained a `.`/`..` path segment outside the recognized
    /// leading `.`/`./`/`~`/`~/` prefix forms.
    #[error("relative pathing '.' and '..' is not allowed in hashFiles pattern {pattern:?}")]
    RelativePathingNotAllowed {
        /// The offending pattern, as written.
        pattern: String,
    },
    /// A filesystem read failed (and wasn't a leniently skipped broken
    /// symlink).
    #[error("hashFiles() failed to read {path}: {source}")]
    Io {
        /// The path that failed to read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A pattern (after rooting) failed to compile as a glob.
    #[error("hashFiles() pattern {pattern:?} is not a valid glob: {source}")]
    InvalidPattern {
        /// The fully-rooted, brace-escaped pattern that failed to compile.
        pattern: String,
        /// The underlying glob-compilation error.
        #[source]
        source: globset::Error,
    },
    /// File discovery or hashing exceeded the runner's fixed deadline.
    #[error("hashFiles('{patterns}') couldn't finish within {seconds} seconds.")]
    Timeout {
        /// The comma-separated, expression-string-escaped glob patterns.
        patterns: String,
        /// The runner-compatible deadline in seconds.
        seconds: u64,
    },
    /// The host could not start the bounded filesystem worker.
    #[error("hashFiles() couldn't start its bounded filesystem worker: {source}")]
    WorkerStart {
        /// The operating-system thread creation failure.
        #[source]
        source: std::io::Error,
    },
    /// The bounded filesystem worker stopped without returning a result.
    #[error("hashFiles() filesystem worker stopped without returning a result")]
    WorkerStopped,
    /// Traversal discovered more distinct resolved directory identities than
    /// the fixed alias registry can retain.
    #[error(
        "hashFiles() stopped after discovering more than {max_directories} directory identities; narrow the patterns to traverse fewer canonical directories"
    )]
    DirectoryCountLimit {
        /// Maximum distinct directory identities retained per invocation.
        max_directories: usize,
    },
    /// Resolved directory identities exceeded the fixed alias-registry byte
    /// budget.
    #[error(
        "hashFiles() stopped after directory identities exceeded {max_bytes} retained bytes; narrow the patterns to traverse fewer canonical directories"
    )]
    DirectoryPathBytesLimit {
        /// Maximum retained directory-identity bytes per invocation.
        max_bytes: usize,
    },
    /// The fixed cap on simultaneously live `hashFiles` filesystem workers
    /// is already reserved — by calls still computing normally, by calls
    /// stuck on an unresponsive host filesystem, or a mix of both; the cap
    /// bounds live worker threads, not specifically stranded ones (finding 3
    /// of the hashFiles hardening re-verification). See ARCHITECTURE.md's
    /// known-issues entry; upstream pattern: [runner hashFiles timeout issue
    /// #1840](https://github.com/actions/runner/issues/1840).
    #[error(
        "hashFiles() couldn't start: {max} filesystem workers are already running (concurrent hashFiles() calls, or a worker stuck on an unresponsive host filesystem); wait for concurrent evaluations to finish, or investigate the workspace's (or $HOME's) filesystem for a stall, before retrying"
    )]
    StrandedWorkerLimit {
        /// The fixed cap on simultaneously live workers.
        max: usize,
    },
    /// A matched symbolic link's target could not be found. GitHub's
    /// pinned hash helper follows the link with `fs.statSync` and exits
    /// nonzero when that fails, which the runner surfaces as a
    /// `hashFiles(...) failed` expression error rather than an empty
    /// match — Greenlit matches that instead of silently excluding it (see
    /// finding 8 of the hashFiles hardening review).
    /// <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Misc/expressionFunc/hashFiles/src/hashFiles.ts>
    /// <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Runner.Worker/Expressions/HashFilesFunction.cs>
    #[error(
        "hashFiles() failed: {path} is a symbolic link with no target; fix or remove the dangling link"
    )]
    DanglingSymlink {
        /// The dangling symlink's path.
        path: String,
    },
}

/// Evaluates `hashFiles(args…)` against `fs`. `args` are the already
/// `ToString`-converted function arguments. The public function contract is
/// documented at
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#hashfiles>;
/// there is no documented laziness for `hashFiles`, unlike `format`/`join`).
pub(crate) fn hash_files(
    args: &[String],
    fs: Arc<dyn HashFilesFs>,
    clock: Arc<dyn HashFilesClock>,
) -> Result<String, HashFilesError> {
    const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(10);

    // The runner starts its timeout before invoking the helper process. Keep
    // one start instant for both the caller-side hard boundary and the
    // worker's cooperative checks, including pattern compilation and the
    // filesystem's own lazy root/`$HOME` establishment (finding 2: neither
    // may happen outside this supervised window).
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Runner.Worker/Expressions/HashFilesFunction.cs
    let started = Instant::now();
    let mut follow_symlinks = false;
    let mut pattern_args = args;
    if let Some(first) = args.first()
        && first.starts_with("--")
    {
        if first.eq_ignore_ascii_case("--follow-symbolic-links") {
            follow_symlinks = true;
            pattern_args = &args[1..];
        } else {
            return Err(HashFilesError::InvalidOption(first.clone()));
        }
    }

    let caller_deadline = HashFilesDeadline::new(pattern_args, clock.as_ref(), started);
    caller_deadline.check()?;

    // A worker stuck in an uninterruptible host syscall can never be killed
    // from within this library — only a process supervisor could do that,
    // out of scope for Phase 1. Bound the damage instead: atomically reserve
    // one live-worker slot before starting the thread, refusing to start
    // another once the fixed cap is already reserved, rather than accumulate
    // unbounded stuck threads (finding 3; see `stranded.rs`).
    let guard = StrandGuard::try_acquire()?;

    let worker_patterns = pattern_args.to_vec();
    let worker_clock = Arc::clone(&clock);
    let worker_guard = Arc::clone(&guard);
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("greenlit-hash-files".to_string())
        .spawn(move || {
            let deadline = HashFilesDeadline::new(&worker_patterns, worker_clock.as_ref(), started);
            let result =
                hash_files_worker(&worker_patterns, fs.as_ref(), follow_symlinks, &deadline);
            // A timed-out caller intentionally drops its receiver and detaches
            // this worker. The result then has nowhere to go; discarding that
            // send failure is the expected cleanup path.
            let _send_result = sender.send(result);
            drop(worker_guard);
        });
    let worker = match worker {
        Ok(worker) => worker,
        Err(source) => {
            caller_deadline.check()?;
            return Err(HashFilesError::WorkerStart { source });
        }
    };
    // Dropping a Rust join handle detaches the worker. This is required for a
    // hard caller deadline when an uninterruptible host filesystem call is
    // still blocked; cooperative checks stop it as soon as that call returns.
    drop(worker);

    loop {
        let wait = match caller_deadline.wait_slice(SUPERVISOR_POLL_INTERVAL) {
            Ok(wait) => wait,
            Err(timeout) => {
                // The caller's own `guard` clone drops here (implicitly, on
                // return); if the worker is still running, its own clone
                // keeps the reserved slot held until it actually exits (see
                // `stranded.rs`).
                return Err(timeout);
            }
        };
        match receiver.recv_timeout(wait) {
            Ok(result) => {
                caller_deadline.check()?;
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                caller_deadline.check()?;
                return Err(HashFilesError::WorkerStopped);
            }
        }
    }
}

fn hash_files_worker(
    pattern_args: &[String],
    fs: &dyn HashFilesFs,
    follow_symlinks: bool,
    deadline: &HashFilesDeadline<'_>,
) -> Result<String, HashFilesError> {
    deadline.check()?;
    let compiled_result = compile_patterns(pattern_args, fs);
    deadline.check()?;
    let compiled = compiled_result?;
    if compiled.is_empty() {
        return Ok(String::new());
    }

    let workspace_root = fs.workspace_root().to_path_buf();
    // The runner's glob generator yields one file at a time and the helper
    // hashes it immediately. Keep the same order and bounded streaming shape
    // instead of retaining every repository path before hashing.
    let mut outer = Sha256::new();
    let mut has_match = false;
    {
        let mut visited_directories = DirectoryRegistry::new();
        let mut visit_file = |path: &Path,
                              source: HashSource<'_, '_>,
                              symlink_sourced: bool|
         -> Result<(), HashFilesError> {
            deadline.check()?;
            if !path.starts_with(&workspace_root) {
                return Ok(());
            }
            let child_sourced = matches!(source, HashSource::Child { .. });
            let mut check_timeout = || deadline.check_io();
            let digest = match source {
                HashSource::EntryPoint => fs.hash_file_sha256(path, &mut check_timeout),
                HashSource::Child {
                    parent,
                    name,
                    follow,
                } => parent.hash_child_file(name, path, follow, &mut check_timeout),
            };
            match digest {
                Ok(inner) => {
                    deadline.check()?;
                    outer.update(inner);
                    has_match = true;
                    Ok(())
                }
                Err(source) if symlink_sourced => {
                    classify_symlink_hash_error(path, source, deadline)
                }
                // Finding 1: a child whose containing directory vanished
                // (deleted, or renamed away — the rename-escape fix turns
                // that into `ENOENT` instead of an escape) between being
                // listed and being read is never a hard failure, the
                // same tolerance `walk_search_root` already gives its own
                // vanished-before-traversal search root.
                Err(source) if child_sourced && source.kind() == std::io::ErrorKind::NotFound => {
                    deadline.check()?;
                    Ok(())
                }
                Err(source) => Err(deadline.map_io(path, source)),
            }
        };
        // The toolkit derives literal roots from positive patterns and visits
        // those roots in pattern order. This order is observable because the
        // outer hash consumes per-file digests in generator order. Greenlit
        // clamps an ancestor search root (notably `$HOME` for `~/**`) down to
        // the workspace and skips unrelated outside roots, preserving every
        // possible in-workspace match without traversing host data that can
        // never be included in the result.
        // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-pattern-helper.ts
        // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
        for root in merged_search_roots(&compiled) {
            deadline.check()?;
            let bounded_root = if root.starts_with(&workspace_root) {
                root
            } else if workspace_root.starts_with(&root) {
                workspace_root.clone()
            } else {
                continue;
            };
            walk_search_root(
                fs,
                &bounded_root,
                follow_symlinks,
                &compiled,
                &mut visit_file,
                &mut visited_directories,
                deadline,
            )?;
        }
    }
    deadline.check()?;

    // Zero matches produce an empty string, including when every match was
    // a broken symlink excluded by containment (see
    // `classify_symlink_hash_error`) rather than a hard failure.
    if !has_match {
        return Ok(String::new());
    }
    Ok(to_hex(&outer.finalize()))
}

/// Classifies a hash failure for a matched symlink-sourced path.
///
/// `ErrorKind::NotFound` means the link's target does not exist: GitHub's
/// pinned helper would fail identically (finding 8), so this becomes a hard
/// `hashFiles(...)` failure rather than a silent empty-string match.
/// `ErrorKind::PermissionDenied` is `RealFs`'s own workspace-containment
/// rejection (see `filesystem/real.rs`'s `outside_workspace`) — a
/// Greenlit-specific security boundary with no GitHub equivalent, so it
/// stays a quiet exclusion exactly as before. Anything else (a corrupted
/// link, an unreadable target, too many nested links, …) is also a hard
/// failure: GitHub's helper would fail too, and only the containment case
/// above is deliberately non-parity.
fn classify_symlink_hash_error(
    path: &Path,
    source: std::io::Error,
    deadline: &HashFilesDeadline<'_>,
) -> Result<(), HashFilesError> {
    let kind = source.kind();
    let mapped = deadline.map_io(path, source);
    // A genuinely expired deadline always wins, matching every other
    // timeout check in this module.
    if matches!(mapped, HashFilesError::Timeout { .. }) {
        return Err(mapped);
    }
    match kind {
        std::io::ErrorKind::PermissionDenied => Ok(()),
        std::io::ErrorKind::NotFound => Err(HashFilesError::DanglingSymlink {
            path: path.display().to_string(),
        }),
        _ => Err(mapped),
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}
