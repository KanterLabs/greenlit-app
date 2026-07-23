//! A global cap on simultaneously stranded `hashFiles` filesystem workers.
//!
//! Finding 3 of the hashFiles hardening review: a worker blocked in an
//! uninterruptible host syscall (a stalled FUSE/NFS mount, for example)
//! cannot be killed from within this library — only a process supervisor
//! could do that, which is out of scope for the Phase 1 engine-core crate.
//! The caller-side deadline in `hash_files.rs` already returns on time by
//! detaching such a worker, but detaching it does not free anything: the
//! thread, its held descriptors, and its filesystem `Arc` all persist until
//! the blocking call eventually returns (if it ever does). Repeated calls
//! against the same stuck filesystem would otherwise accumulate an
//! unbounded number of these permanently stranded workers. Containment:
//! once `MAX_STRANDED_WORKERS` previous workers are still stranded, a new
//! `hashFiles()` call fails fast with the corrective action instead of
//! starting yet another worker likely to hang on the same broken
//! filesystem. See ARCHITECTURE.md's known-issues entry and the upstream
//! pattern this mirrors: [runner hashFiles timeout issue
//! #1840](https://github.com/actions/runner/issues/1840).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::HashFilesError;

const MAX_STRANDED_WORKERS: usize = 4;

static STRANDED_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// One call's stake in the global stranded-worker cap. Shared between the
/// caller (which decides whether the worker became stranded) and the
/// worker thread (whose eventual exit — however late — releases it).
pub(super) struct StrandGuard {
    marked: AtomicBool,
}

impl StrandGuard {
    /// Reserves nothing up front (a call that completes on time never
    /// counts against the cap); it only refuses to start when the cap is
    /// already reached by *previous* stranded workers.
    pub(super) fn try_acquire() -> Result<Arc<Self>, HashFilesError> {
        if STRANDED_WORKERS.load(Ordering::Acquire) >= MAX_STRANDED_WORKERS {
            return Err(HashFilesError::StrandedWorkerLimit {
                max: MAX_STRANDED_WORKERS,
            });
        }
        Ok(Arc::new(Self {
            marked: AtomicBool::new(false),
        }))
    }

    /// Called by the caller-side supervisor the moment it gives up waiting
    /// (the worker has not delivered a result). From this point the worker,
    /// if still blocked, is stranded until it eventually exits.
    pub(super) fn mark_stranded(&self) {
        if !self.marked.swap(true, Ordering::AcqRel) {
            STRANDED_WORKERS.fetch_add(1, Ordering::AcqRel);
        }
    }
}

impl Drop for StrandGuard {
    fn drop(&mut self) {
        // Only a call that was actually marked stranded incremented the
        // counter; a call that completed on time never touches it. The last
        // `Arc<StrandGuard>` clone to drop is always the worker thread's own
        // (the caller's clone is dropped first, whether or not it marked
        // this guard), so the decrement fires exactly when the stranded
        // worker itself finally exits.
        if self.marked.load(Ordering::Acquire) {
            STRANDED_WORKERS.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
