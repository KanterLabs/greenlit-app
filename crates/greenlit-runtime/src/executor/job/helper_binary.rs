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
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, fchmod, openat, renameat_with, unlinkat};
use rustix::io::Errno;
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
    let directory_handle = open_private_runtime_directory(state_root)?;
    let digest = digest_hex(init_binary());
    let published_name = format!("greenlit-init-{digest}");
    let published = directory.join(&published_name);
    if let Some(mut file) = open_private_helper(&directory_handle, &published, &published_name)? {
        if helper_matches(&mut file, &digest)? {
            return Ok(published);
        }
        return Err(mismatched_helper_error(&published));
    }

    let temporary_name = format!(
        ".greenlit-init-{}-{}.partial",
        std::process::id(),
        unique_suffix()
    );
    let temporary = directory.join(&temporary_name);
    let published_new = match publish(
        &directory_handle,
        &temporary_name,
        &temporary,
        &published_name,
    ) {
        Ok(true) => true,
        Ok(false) => {
            remove_temporary(&directory_handle, &temporary_name)?;
            false
        }
        Err(primary) => {
            if let Err(cleanup) = remove_temporary(&directory_handle, &temporary_name) {
                return Err(staging_error(&format!(
                    "{primary}; additionally, private helper staging cleanup failed: {cleanup}"
                )));
            }
            return Err(primary);
        }
    };
    if !published_new {
        let Some(mut file) = open_private_helper(&directory_handle, &published, &published_name)?
        else {
            return Err(staging_error(
                "the helper publication raced with a path that disappeared; retry the run",
            ));
        };
        if !helper_matches(&mut file, &digest)? {
            return Err(mismatched_helper_error(&published));
        }
    }
    Ok(published)
}

fn remove_temporary(directory: &File, name: &str) -> Result<(), ExecError> {
    match unlinkat(directory, name, AtFlags::empty()) {
        Ok(()) => directory.sync_all().map_err(io_error),
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(io_error(error.into())),
    }
}

/// Stage a unique helper for runtime-library callers that configured no store.
fn stage_ephemeral() -> Result<PathBuf, ExecError> {
    let path = std::env::temp_dir().join(format!(
        "greenlit-init-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    write_ephemeral_file(&path)?;
    Ok(path)
}

/// Atomically move a fully written helper into its immutable public name.
fn publish(
    directory: &File,
    temporary_name: &str,
    temporary: &Path,
    published_name: &str,
) -> Result<bool, ExecError> {
    write_private_helper(directory, temporary_name, temporary)?;
    match renameat_with(
        directory,
        temporary_name,
        directory,
        published_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {
            directory.sync_all().map_err(io_error)?;
            Ok(true)
        }
        Err(Errno::EXIST) => Ok(false),
        Err(error) => Err(io_error(error.into())),
    }
}

/// Write and synchronize the embedded helper with executable permissions.
fn write_ephemeral_file(path: &Path) -> Result<(), ExecError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(path)
        .map_err(io_error)?;
    normalize_new_helper(path, &file)?;
    file.write_all(init_binary()).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn open_private_runtime_directory(state_root: &Path) -> Result<File, ExecError> {
    super::super::private_state::ensure_directory(state_root, Path::new("runtime"))
        .map_err(io_error)
}

fn write_private_helper(directory: &File, name: &str, path: &Path) -> Result<(), ExecError> {
    let mut file = openat(
        directory,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    )
    .map(File::from)
    .map_err(|error| io_error(error.into()))?;
    normalize_new_helper(path, &file)?;
    file.write_all(init_binary()).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn open_private_helper(
    directory: &File,
    path: &Path,
    name: &str,
) -> Result<Option<File>, ExecError> {
    let file = match openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(io_error(error.into())),
    };
    validate_private_helper(path, &file, 0o700)?;
    Ok(Some(file))
}

fn helper_matches(file: &mut File, digest: &str) -> Result<bool, ExecError> {
    let metadata = file.metadata().map_err(io_error)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(file, &mut bytes).map_err(io_error)?;
    Ok(metadata.len() == init_binary().len() as u64 && digest_hex(&bytes) == digest)
}

fn mismatched_helper_error(path: &Path) -> ExecError {
    staging_error(&format!(
        "{} is private but its bytes do not match the embedded helper digest; remove this file and retry",
        path.display()
    ))
}

fn normalize_new_helper(path: &Path, file: &File) -> Result<(), ExecError> {
    let metadata = file.metadata().map_err(io_error)?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.nlink() != 1 || mode & !0o700 != 0 {
        return Err(staging_error(&format!(
            "{} has unsafe new-file mode 0{mode:03o} or link count {}",
            path.display(),
            metadata.nlink()
        )));
    }
    fchmod(file, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(|error| io_error(error.into()))?;
    validate_private_helper(path, file, 0o700)
}

fn validate_private_helper(path: &Path, file: &File, expected: u32) -> Result<(), ExecError> {
    let metadata = file.metadata().map_err(io_error)?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.nlink() != 1 || mode != expected {
        return Err(staging_error(&format!(
            "{} has unsafe file type, mode 0{mode:03o}, or link count {}",
            path.display(),
            metadata.nlink()
        )));
    }
    Ok(())
}

fn validate_owner(path: &Path, metadata: &std::fs::Metadata) -> Result<(), ExecError> {
    let current_uid = rustix::process::getuid().as_raw();
    if metadata.uid() == current_uid {
        Ok(())
    } else {
        Err(staging_error(&format!(
            "{} is owned by uid {}, not current uid {current_uid}",
            path.display(),
            metadata.uid()
        )))
    }
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
