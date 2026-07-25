//! Machine-wide crash-safe worker admission.
//!
//! Each worker owns an advisory lock on one fixed slot file. The kernel
//! releases that lock if the process crashes, so there is no stale lease to
//! repair. A run may claim at most all but one machine slot, preventing one
//! project from indefinitely occupying the entire executor while retaining
//! full capacity when only one slot exists.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rustix::fs::{FlockOperation, flock};

use super::ExecError;

static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

pub(super) struct MachineWorkerPool {
    directory: PathBuf,
    slots: usize,
}

pub(super) struct MachineWorkerPermit {
    _file: File,
}

impl MachineWorkerPool {
    pub(super) async fn open(state_root: &Path, slots: usize) -> Result<Self, ExecError> {
        let directory = state_root.join("scheduler").join("v1").join("slots");
        let task_directory = directory.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(task_directory))
            .await
            .map_err(|error| infrastructure(format!("worker-pool setup stopped: {error}")))?
            .map_err(|error| {
                infrastructure(format!(
                    "could not create the machine worker-pool directory: {error}"
                ))
            })?;
        Ok(Self {
            directory,
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
            let slots = self.slots;
            let permit = tokio::task::spawn_blocking(move || try_acquire(&directory, slots, start))
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
    slots: usize,
    start: usize,
) -> std::io::Result<Option<MachineWorkerPermit>> {
    for offset in 0..slots {
        let index = (start + offset) % slots;
        let path = directory.join(format!("{index:03}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Some(MachineWorkerPermit { _file: file })),
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => {}
            Err(error) => return Err(std::io::Error::from_raw_os_error(error.raw_os_error())),
        }
    }
    Ok(None)
}

fn infrastructure(message: String) -> ExecError {
    ExecError::Infrastructure {
        message,
        fix: "check permissions on ~/.litci/scheduler, then retry".to_string(),
    }
}
