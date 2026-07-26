//! Durable host staging for the private `greenlit-init` helper.
//!
//! Docker resolves bind sources in the daemon's mount namespace when a
//! container starts. A helper written into a runner-managed scratch directory
//! can disappear between create and start if that directory is swept between
//! CI steps. Normal Greenlit runs therefore publish the embedded helper under
//! the user-local Greenlit state root, keyed by its digest. The temporary-file
//! fallback exists only for callers that deliberately configure no store.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::executor::ExecError;
use crate::image::init_binary;

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Publish the embedded helper and return its absolute bind source.
pub(super) fn stage(state_root: Option<&Path>) -> Result<String, ExecError> {
    let path = match state_root {
        Some(root) => stage_durable(root)?,
        None => stage_ephemeral()?,
    };
    path.into_os_string()
        .into_string()
        .map_err(|_| staging_error("the helper path is not valid UTF-8"))
}

/// Publish one immutable, digest-addressed helper under Greenlit state.
fn stage_durable(state_root: &Path) -> Result<PathBuf, ExecError> {
    let directory = state_root.join("runtime");
    std::fs::create_dir_all(&directory).map_err(io_error)?;
    let digest = digest_hex(init_binary());
    let published = directory.join(format!("greenlit-init-{digest}"));
    match std::fs::symlink_metadata(&published) {
        Ok(metadata) if metadata.file_type().is_file() => {
            if metadata.len() == init_binary().len() as u64
                && digest_hex(&std::fs::read(&published).map_err(io_error)?) == digest
            {
                ensure_executable(&published)?;
                return Ok(published);
            }
        }
        Ok(_) => {
            return Err(staging_error(&format!(
                "{} exists but is not a regular file; remove it and retry",
                published.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
    }

    let temporary = directory.join(format!(
        ".greenlit-init-{}-{}.partial",
        std::process::id(),
        unique_suffix()
    ));
    let result = publish(&temporary, &published);
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(published)
}

/// Stage a unique helper for runtime-library callers that configured no store.
fn stage_ephemeral() -> Result<PathBuf, ExecError> {
    let path = std::env::temp_dir().join(format!(
        "greenlit-init-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    write_file(&path)?;
    Ok(path)
}

/// Atomically move a fully written helper into its immutable public name.
fn publish(temporary: &Path, published: &Path) -> Result<(), ExecError> {
    write_file(temporary)?;
    std::fs::rename(temporary, published).map_err(io_error)?;
    Ok(())
}

/// Write and synchronize the embedded helper with executable permissions.
fn write_file(path: &Path) -> Result<(), ExecError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(io_error)?;
    file.write_all(init_binary()).map_err(io_error)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o755))
        .map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

/// Repair executable mode after an overzealous external permissions sweep.
fn ensure_executable(path: &Path) -> Result<(), ExecError> {
    let file = File::open(path).map_err(io_error)?;
    let mut permissions = file.metadata().map_err(io_error)?.permissions();
    if permissions.mode() & 0o111 == 0 {
        permissions.set_mode(0o755);
        file.set_permissions(permissions).map_err(io_error)?;
    }
    Ok(())
}

fn unique_suffix() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp}-{sequence}")
}

fn digest_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}

fn io_error(source: std::io::Error) -> ExecError {
    staging_error(&source.to_string())
}

fn staging_error(detail: &str) -> ExecError {
    ExecError::Infrastructure {
        message: format!("could not stage the greenlit-init helper: {detail}"),
        fix: "ensure the Greenlit state directory is writable, then retry".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::stage;

    #[test]
    fn durable_helper_is_stable_and_executable() {
        let root = tempfile::tempdir().expect("temp state");
        let first = stage(Some(root.path())).expect("first stage");
        let second = stage(Some(root.path())).expect("second stage");

        assert_eq!(first, second);
        let mode = std::fs::metadata(first)
            .expect("helper metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }

    #[test]
    fn durable_helper_rejects_directory_at_digest_path() {
        let root = tempfile::tempdir().expect("temp state");
        let path = stage(Some(root.path())).expect("initial stage");
        std::fs::remove_file(&path).expect("remove helper");
        std::fs::create_dir(&path).expect("replace helper with directory");

        let error = stage(Some(root.path())).expect_err("directory must be rejected");
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn durable_helper_atomically_repairs_corrupt_content() {
        let root = tempfile::tempdir().expect("temp state");
        let path = stage(Some(root.path())).expect("initial stage");
        std::fs::write(&path, b"corrupt").expect("corrupt helper");

        let repaired = stage(Some(root.path())).expect("repair helper");

        assert_eq!(repaired, path);
        assert_eq!(
            std::fs::read(repaired).expect("repaired bytes"),
            crate::image::init_binary()
        );
    }
}
