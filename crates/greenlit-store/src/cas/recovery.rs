//! Fail-closed reconciliation for interrupted retained-run publication.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{
    AtFlags, CWD, FlockOperation, Mode, OFlags, RenameFlags, ResolveFlags, chmod, fchmod, flock,
    mkdirat, openat, openat2, renameat_with, statat,
};
use rustix::io::Errno;

use super::{CasError, CasStore, RunCatalogState, io_error};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const PUBLICATION_LOCK: &str = "publication.lock";
const QUARANTINE_DIRECTORY: &str = ".recovery-quarantine";

/// Descriptor-held exclusive liveness proof for one active run publisher.
pub struct RunPublicationGuard {
    run_id: String,
    _file: File,
}

impl std::fmt::Debug for RunPublicationGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunPublicationGuard")
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

/// Current state of a retained run's descriptor-held publication lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPublicationLockState {
    /// Another process holds the exclusive publication lock.
    Active,
    /// The lock exists and can be acquired, proving no publisher holds it.
    Inactive,
    /// The run tree or lock is absent, so inactivity cannot be inferred from
    /// an advisory lock.
    Missing,
}

/// One inactive run whose incomplete publication was revoked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredRun {
    /// Stable run identity.
    pub run_id: String,
    /// State observed before this recovery pass.
    pub previous_state: RunCatalogState,
    /// Whether a retained tree was moved out of the authoritative run set.
    pub quarantined: bool,
}

/// Result of one incomplete-publication recovery pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecoveryReport {
    /// Runs revoked during this pass, including cleanup retries for rows
    /// already marked aborted.
    pub recovered: Vec<RecoveredRun>,
    /// Noncompleted runs left untouched because their publication locks are
    /// held.
    pub active: Vec<String>,
    /// Existing run trees left untouched because they predate or lack the
    /// descriptor-held liveness protocol.
    pub unprotected: Vec<String>,
}

pub(super) fn acquire_publication_guard(
    runs_root: &Path,
    run_id: &str,
) -> Result<RunPublicationGuard, CasError> {
    let runs = open_required_private_directory(runs_root)?;
    let run = open_run_directory(&runs, runs_root, run_id)?.ok_or_else(|| {
        io_error(
            &runs_root.join(run_id),
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "retained run directory does not exist",
            ),
        )
    })?;
    let path = runs_root.join(run_id).join(PUBLICATION_LOCK);
    let file = openat(
        &run,
        PUBLICATION_LOCK,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)
    .map_err(|error| io_error(&path, error.into()))?;
    validate_new_private_file(&path, &file)?;
    fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(|error| io_error(&path, error.into()))?;
    validate_private_file(&path, &file)?;
    flock(&file, FlockOperation::LockExclusive).map_err(|error| io_error(&path, error.into()))?;
    file.sync_all().map_err(|error| io_error(&path, error))?;
    run.sync_all()
        .map_err(|error| io_error(&runs_root.join(run_id), error))?;
    Ok(RunPublicationGuard {
        run_id: run_id.to_string(),
        _file: file,
    })
}

pub(super) fn publication_lock_state(
    runs_root: &Path,
    run_id: &str,
) -> Result<RunPublicationLockState, CasError> {
    let Some(runs) = open_optional_private_directory(runs_root)? else {
        return Ok(RunPublicationLockState::Missing);
    };
    Ok(match probe_publication_lock(&runs, runs_root, run_id)? {
        LockProbe::Active => RunPublicationLockState::Active,
        LockProbe::Acquired(_guard) => RunPublicationLockState::Inactive,
        LockProbe::MissingLock | LockProbe::MissingRun => RunPublicationLockState::Missing,
    })
}

pub(super) fn recover(store: &CasStore, runs_root: &Path) -> Result<RunRecoveryReport, CasError> {
    let runs = open_optional_private_directory(runs_root)?;
    let mut report = RunRecoveryReport {
        recovered: Vec::new(),
        active: Vec::new(),
        unprotected: Vec::new(),
    };

    for entry in store.run_catalog_entries()? {
        if entry.state == RunCatalogState::Completed {
            continue;
        }
        let probe = match runs.as_ref() {
            Some(runs) => probe_publication_lock(runs, runs_root, &entry.run_id)?,
            None => LockProbe::MissingRun,
        };
        match probe {
            LockProbe::Active => report.active.push(entry.run_id),
            LockProbe::MissingLock => report.unprotected.push(entry.run_id),
            LockProbe::MissingRun => report.unprotected.push(entry.run_id),
            LockProbe::Acquired(_guard) => {
                if entry.state == RunCatalogState::Resolved
                    && !store.catalog.abort_if_incomplete(&entry.run_id)?
                {
                    continue;
                }
                let quarantined = match runs.as_ref() {
                    Some(runs) => quarantine_run_tree(runs, runs_root, &entry.run_id)?,
                    _ => false,
                };
                if entry.state == RunCatalogState::Resolved || quarantined {
                    report.recovered.push(RecoveredRun {
                        run_id: entry.run_id,
                        previous_state: entry.state,
                        quarantined,
                    });
                }
            }
        }
    }
    Ok(report)
}

enum LockProbe {
    Active,
    Acquired(RunPublicationGuard),
    MissingLock,
    MissingRun,
}

fn probe_publication_lock(
    runs: &File,
    runs_root: &Path,
    run_id: &str,
) -> Result<LockProbe, CasError> {
    let Some(run) = open_run_directory(runs, runs_root, run_id)? else {
        return Ok(LockProbe::MissingRun);
    };
    let path = runs_root.join(run_id).join(PUBLICATION_LOCK);
    let file = match openat(
        &run,
        PUBLICATION_LOCK,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(LockProbe::MissingLock),
        Err(error) => return Err(io_error(&path, error.into())),
    };
    validate_private_file(&path, &file)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(LockProbe::Acquired(RunPublicationGuard {
            run_id: run_id.to_string(),
            _file: file,
        })),
        Err(Errno::WOULDBLOCK) => Ok(LockProbe::Active),
        Err(error) => Err(io_error(&path, error.into())),
    }
}

fn open_required_private_directory(path: &Path) -> Result<File, CasError> {
    open_optional_private_directory(path)?.ok_or_else(|| {
        io_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "retained runs directory does not exist",
            ),
        )
    })
}

fn open_optional_private_directory(path: &Path) -> Result<Option<File>, CasError> {
    match openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
    ) {
        Ok(directory) => {
            let directory = File::from(directory);
            validate_private_directory(path, &directory)?;
            Ok(Some(directory))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(io_error(path, error.into())),
    }
}

fn open_run_directory(
    runs: &File,
    runs_root: &Path,
    run_id: &str,
) -> Result<Option<File>, CasError> {
    let path = runs_root.join(run_id);
    match openat(
        runs,
        run_id,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(run) => {
            let run = File::from(run);
            validate_private_directory(&path, &run)?;
            Ok(Some(run))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(io_error(&path, error.into())),
    }
}

fn quarantine_run_tree(runs: &File, runs_path: &Path, run_id: &str) -> Result<bool, CasError> {
    if !run_entry_exists(runs, runs_path, run_id)? {
        return Ok(false);
    }
    let quarantine = ensure_quarantine_directory(runs, runs_path)?;
    let destination = runs_path.join(QUARANTINE_DIRECTORY).join(run_id);
    match renameat_with(runs, run_id, &quarantine, run_id, RenameFlags::NOREPLACE) {
        Ok(()) => {
            runs.sync_all()
                .map_err(|error| io_error(runs_path, error))?;
            quarantine
                .sync_all()
                .map_err(|error| io_error(&destination, error))?;
            Ok(true)
        }
        Err(Errno::NOENT) => Ok(false),
        Err(error) => Err(io_error(&destination, error.into())),
    }
}

fn run_entry_exists(runs: &File, runs_path: &Path, run_id: &str) -> Result<bool, CasError> {
    match statat(runs, run_id, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(Errno::NOENT) => Ok(false),
        Err(error) => Err(io_error(&runs_path.join(run_id), error.into())),
    }
}

fn ensure_quarantine_directory(runs: &File, runs_path: &Path) -> Result<File, CasError> {
    let path = runs_path.join(QUARANTINE_DIRECTORY);
    match mkdirat(
        runs,
        QUARANTINE_DIRECTORY,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    ) {
        Ok(()) => {
            let inspected = openat(
                runs,
                QUARANTINE_DIRECTORY,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|error| io_error(&path, error.into()))?;
            chmod_descriptor(&inspected, &path)?;
            runs.sync_all()
                .map_err(|error| io_error(runs_path, error))?;
        }
        Err(Errno::EXIST) => {}
        Err(error) => return Err(io_error(&path, error.into())),
    }
    let directory = openat(
        runs,
        QUARANTINE_DIRECTORY,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error(&path, error.into()))?;
    validate_private_directory(&path, &directory)?;
    Ok(directory)
}

fn validate_private_directory(path: &Path, directory: &File) -> Result<(), CasError> {
    let metadata = directory
        .metadata()
        .map_err(|error| io_error(path, error))?;
    let current_uid = rustix::process::getuid().as_raw();
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir() || metadata.uid() != current_uid || mode != PRIVATE_DIRECTORY_MODE {
        return Err(io_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "refused unsafe retained-run directory: owner uid {}, mode 0{mode:03o}",
                    metadata.uid()
                ),
            ),
        ));
    }
    Ok(())
}

fn validate_new_private_file(path: &Path, file: &File) -> Result<(), CasError> {
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    let current_uid = rustix::process::getuid().as_raw();
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file()
        || metadata.uid() != current_uid
        || metadata.nlink() != 1
        || mode & !PRIVATE_FILE_MODE != 0
    {
        return Err(io_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "refused unsafe new publication lock: owner uid {}, mode 0{mode:03o}, links {}",
                    metadata.uid(),
                    metadata.nlink()
                ),
            ),
        ));
    }
    Ok(())
}

fn validate_private_file(path: &Path, file: &File) -> Result<(), CasError> {
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    let current_uid = rustix::process::getuid().as_raw();
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file()
        || metadata.uid() != current_uid
        || metadata.nlink() != 1
        || mode != PRIVATE_FILE_MODE
    {
        return Err(io_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "refused unsafe publication lock: owner uid {}, mode 0{mode:03o}, links {}",
                    metadata.uid(),
                    metadata.nlink()
                ),
            ),
        ));
    }
    Ok(())
}

fn chmod_descriptor(directory: &File, path: &Path) -> Result<(), CasError> {
    let descriptor = format!("/proc/self/fd/{}", directory.as_raw_fd());
    chmod(descriptor, Mode::RUSR | Mode::WUSR | Mode::XUSR)
        .map_err(|error| io_error(path, error.into()))
}
