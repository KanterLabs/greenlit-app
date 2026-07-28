//! Machine-wide crash-safe worker admission.
//!
//! Each worker owns an advisory lock on one fixed slot file. The kernel
//! releases that lock if the process crashes, so there is no stale lease to
//! repair. A run may claim at most all but one machine slot, preventing one
//! project from indefinitely occupying the entire executor while retaining
//! full capacity when only one slot exists.

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rustix::fs::{FlockOperation, Mode, OFlags, fchmod, flock, openat};
use rustix::io::Errno;

use super::ExecError;

static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

pub(super) struct MachineWorkerPool {
    directory: PathBuf,
    directory_handle: Arc<File>,
    slots: usize,
}

pub(super) struct MachineWorkerPermit {
    _file: File,
}

impl MachineWorkerPool {
    pub(super) async fn open(state_root: &Path, slots: usize) -> Result<Self, ExecError> {
        let directory = state_root.join("scheduler").join("v1").join("slots");
        let task_root = state_root.to_path_buf();
        let directory_handle =
            tokio::task::spawn_blocking(move || open_private_scheduler_directory(&task_root))
                .await
                .map_err(|error| infrastructure(format!("worker-pool setup stopped: {error}")))?
                .map_err(|error| {
                    infrastructure(format!(
                        "could not create the machine worker-pool directory: {error}"
                    ))
                })?;
        Ok(Self {
            directory,
            directory_handle: Arc::new(directory_handle),
            slots: slots.max(1),
        })
    }

    pub(super) async fn acquire(
        &self,
        cancellation: &crate::Cancellation,
    ) -> Result<MachineWorkerPermit, ExecError> {
        let start = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
        loop {
            let directory = self.directory.clone();
            let directory_handle = Arc::clone(&self.directory_handle);
            let slots = self.slots;
            let permit = tokio::task::spawn_blocking(move || {
                try_acquire(&directory, &directory_handle, slots, start)
            })
            .await
            .map_err(|error| infrastructure(format!("worker-slot admission stopped: {error}")))?
            .map_err(|error| {
                infrastructure(format!("could not inspect machine worker slots: {error}"))
            })?;
            if let Some(permit) = permit {
                return Ok(permit);
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
                () = cancellation.cancelled() => {
                    return Err(ExecError::Infrastructure {
                        message: "run cancellation reached the machine worker queue".to_string(),
                        fix: "retry the run when ready".to_string(),
                    });
                }
            }
        }
    }
}

fn try_acquire(
    directory: &Path,
    directory_handle: &File,
    slots: usize,
    start: usize,
) -> std::io::Result<Option<MachineWorkerPermit>> {
    for offset in 0..slots {
        let index = (start + offset) % slots;
        let path = directory.join(format!("{index:03}.lock"));
        let file =
            open_or_create_private_slot(directory_handle, &path, &format!("{index:03}.lock"))?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Some(MachineWorkerPermit { _file: file })),
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => {}
            Err(error) => return Err(std::io::Error::from_raw_os_error(error.raw_os_error())),
        }
    }
    Ok(None)
}

fn open_private_scheduler_directory(state_root: &Path) -> std::io::Result<File> {
    super::private_state::ensure_directory(state_root, Path::new("scheduler/v1/slots"))
}

fn open_or_create_private_slot(directory: &File, path: &Path, name: &str) -> std::io::Result<File> {
    let existing = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    loop {
        match openat(directory, name, existing, Mode::empty()) {
            Ok(file) => {
                let file = File::from(file);
                validate_private_file(path, &file)?;
                return Ok(file);
            }
            Err(Errno::NOENT) => match openat(
                directory,
                name,
                existing | OFlags::CREATE | OFlags::EXCL,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(file) => {
                    let file = File::from(file);
                    let metadata = file.metadata()?;
                    validate_owner(path, &metadata)?;
                    let mode = metadata.mode() & 0o7777;
                    if !metadata.is_file() || metadata.nlink() != 1 || mode & !0o600 != 0 {
                        return Err(unsafe_inode(
                            path,
                            format!(
                                "new slot has mode 0{mode:03o} and link count {}",
                                metadata.nlink()
                            ),
                        ));
                    }
                    fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(std::io::Error::from)?;
                    validate_private_file(path, &file)?;
                    return Ok(file);
                }
                Err(Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            },
            Err(error) => return Err(error.into()),
        }
    }
}

fn validate_private_file(path: &Path, file: &File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    validate_owner(path, &metadata)?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.nlink() != 1 || mode != 0o600 {
        return Err(unsafe_inode(
            path,
            format!(
                "slot has unsafe type, mode 0{mode:03o}, or link count {}",
                metadata.nlink()
            ),
        ));
    }
    Ok(())
}

fn validate_owner(path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<()> {
    let current_uid = rustix::process::getuid().as_raw();
    if metadata.uid() == current_uid {
        Ok(())
    } else {
        Err(unsafe_inode(
            path,
            format!(
                "path is owned by uid {}, not current uid {current_uid}",
                metadata.uid()
            ),
        ))
    }
}

fn unsafe_inode(path: &Path, detail: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "refused unsafe worker-pool path {}: {detail}",
            path.display()
        ),
    )
}

fn infrastructure(message: String) -> ExecError {
    ExecError::Infrastructure {
        message,
        fix: "check permissions on ~/.litci/scheduler, then retry".to_string(),
    }
}
