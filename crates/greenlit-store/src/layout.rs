//! The on-disk conventions every store in this crate shares.
//!
//! The cache and artifact stores have the same three problems — allocate an
//! id no concurrent writer can also get, create a directory, and turn a
//! caller-chosen name into something safe to use as a path component — and
//! solving them twice would let the two drift.

use std::fs;
use std::path::Path;

use crate::error::StoreError;

/// Creates `path` and every missing parent.
///
/// # Errors
/// Returns [`StoreError::Io`] if the directory cannot be created.
pub(crate) fn create_dir_all(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|source| StoreError::Io {
        operation: "create the store directory",
        path: path.to_path_buf(),
        source,
    })
}

/// Allocates the lowest unused id by *creating* its directory under
/// `pending`, skipping any id already taken in `pending` or `committed`.
///
/// `create_dir` fails when the directory already exists, which makes the
/// create itself the lock: two processes racing for the same id cannot both
/// succeed, and the loser simply tries the next one. That holds across
/// processes, so two concurrent `litci run` invocations sharing one store
/// never collide.
///
/// # Errors
/// Returns [`StoreError::Io`] if a directory cannot be created for a reason
/// other than already existing.
pub(crate) fn allocate_id(pending: &Path, committed: &Path) -> Result<i64, StoreError> {
    let mut next = highest_id(pending).max(highest_id(committed)) + 1;
    loop {
        let candidate = pending.join(next.to_string());
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(next),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                next += 1;
            }
            Err(source) => {
                return Err(StoreError::Io {
                    operation: "create the store entry directory",
                    path: candidate,
                    source,
                });
            }
        }
    }
}

/// The highest numeric directory name under `dir`, or `0` when there is none.
fn highest_id(dir: &Path) -> i64 {
    let Ok(read) = fs::read_dir(dir) else {
        return 0;
    };
    read.flatten()
        .filter_map(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<i64>().ok())
        })
        .max()
        .unwrap_or(0)
}

/// A stable, filesystem-safe digest of a caller-chosen name.
///
/// Cache keys, artifact names, and Azure block ids all originate outside
/// Greenlit and may contain separators, `..`, or anything else; hashing them
/// means no such value ever becomes a path component. FNV-1a is used for the
/// same reason `greenlit-runtime`'s image content hash does: it is
/// dependency-free and deterministic across toolchains, unlike
/// `DefaultHasher`. This is an identity key, not a security boundary — the
/// values it covers are already scoped to a directory the caller controls.
pub(crate) fn name_hash(name: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}
