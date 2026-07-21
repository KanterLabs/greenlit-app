//! `hashFiles(pattern…)`: glob matching plus GitHub's documented two-level
//! SHA-256 algorithm.
//!
//! Source: design memo §3.8, itself derived from
//! `src/Runner.Worker/Expressions/HashFilesFunction.cs`,
//! `src/Misc/expressionFunc/hashFiles/src/hashFiles.ts`, and
//! `@actions/toolkit` `packages/glob/src/internal-*.ts`. Filesystem access is
//! behind the [`HashFilesFs`] trait specifically so tests can supply an
//! in-memory fake while [`RealFs`] ships the real, `std::fs`-backed
//! implementation now (per the Phase 1 task list).
//!
//! Traversal order and symlink behavior follow current toolkit source.
//! Leading-`/` patterns follow the public expressions documentation and are
//! rooted at the workspace; the oracle table pins that user-visible rule.

mod filesystem;
mod patterns;
mod traversal;

#[cfg(test)]
pub(crate) mod test_support;

use std::collections::HashSet;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use self::patterns::{compile_patterns, is_included, merged_search_roots};
use self::traversal::walk_search_root;
use crate::value::Value;

pub use self::filesystem::{DirEntry, EntryKind, HashFilesFs, RealFs};

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
}

/// Evaluates `hashFiles(args…)` against `fs`. `args` are the already
/// `ToString`-converted function arguments (see the design memo §3.8:
/// "1-255 string arguments (each evaluated then ToString)" — there is no
/// documented laziness for `hashFiles`, unlike `format`/`join`).
pub(crate) fn hash_files(args: &[String], fs: &dyn HashFilesFs) -> Result<Value, HashFilesError> {
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

    let compiled = compile_patterns(pattern_args, fs)?;
    if compiled.is_empty() {
        return Ok(Value::String(String::new()));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut symlink_sourced: HashSet<PathBuf> = HashSet::new();
    // The toolkit derives literal roots from positive patterns and visits
    // those roots in pattern order. This order is observable because the
    // outer hash consumes per-file digests in generator order.
    // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-pattern-helper.ts
    // https://github.com/actions/toolkit/blob/main/packages/glob/src/internal-globber.ts
    for root in merged_search_roots(&compiled) {
        walk_search_root(
            fs,
            &root,
            follow_symlinks,
            &mut candidates,
            &mut symlink_sourced,
        )?;
    }
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    candidates.retain(|path| seen_paths.insert(path.clone()));

    let workspace_root = fs.workspace_root().to_path_buf();
    let ordered_paths: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|path| is_included(&compiled, path) && path.starts_with(&workspace_root))
        .collect();

    let mut valid_file_bytes: Vec<Vec<u8>> = Vec::with_capacity(ordered_paths.len());
    for path in &ordered_paths {
        match fs.read_file(path) {
            Ok(bytes) => valid_file_bytes.push(bytes),
            // Broken symlinks are omitted, but only for paths reached by
            // following a symlink; a genuine unreadable file is an error.
            Err(_) if symlink_sourced.contains(path) => continue,
            Err(source) => {
                return Err(HashFilesError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
    }

    // Zero matches produce an empty string, including when every match was
    // a broken symlink.
    if valid_file_bytes.is_empty() {
        return Ok(Value::String(String::new()));
    }

    // SHA-256 each file's raw bytes, feed the raw 32-byte digests into one
    // outer SHA-256 in traversal order, then return lowercase hexadecimal.
    let mut outer = Sha256::new();
    for bytes in &valid_file_bytes {
        let inner = Sha256::digest(bytes);
        outer.update(inner.as_slice());
    }
    Ok(Value::String(to_hex(outer.finalize().as_slice())))
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
