//! A global cap on simultaneously live `hashFiles` filesystem workers.
//!
//! Finding 3 of the hashFiles hardening re-verification: a worker blocked in
//! an uninterruptible host syscall (a stalled FUSE/NFS mount, for example)
//! cannot be killed from within this library — only a process supervisor
//! could do that, which is out of scope for the Phase 1 engine-core crate.
//! The caller-side deadline in `hash_files.rs` already returns on time by
//! detaching such a worker, but detaching it does not free anything: the
//! thread, its held descriptors, and its filesystem `Arc` all persist until
//! the blocking call eventually returns (if it ever does).
//!
//! An earlier revision reserved a permit only once the caller-side
//! supervisor had already given up waiting (`mark_stranded`), incrementing
//! the live counter *after* the worker had been running for a while. That
//! left an unbounded window at call start: arbitrarily many concurrent
//! `hashFiles()` calls could each observe the counter still at zero, spawn a
//! worker, and only *later* be individually detected as stranded — so the
//! cap bounded nothing at the moment it mattered. The fix reserves a permit
//! with a single atomic compare-and-swap (`fetch_update`) *before* the
//! worker thread is spawned, so the counter genuinely bounds the number of
//! concurrently live worker threads (whether they finish on time or end up
//! stranded), not merely the number already detected as stuck. The cap stays
//! small: concurrent `hashFiles()` evaluations are rare during planning, and
//! a small bound keeps a burst of them from spawning unbounded threads even
//! when every one of them would otherwise complete quickly.
//!
//! Release is unconditional on `Drop`, shared via `Arc` between the caller
//! (which drops its clone the moment it stops waiting, whether the worker
//! finished or was abandoned) and the worker thread itself (which drops its
//! clone only when its closure actually returns). Because `Drop::drop` for
//! the guard runs exactly once — when the *last* `Arc` clone goes away —
//! this one mechanism yields both required behaviors for free: a worker
//! that finishes on time releases its permit essentially immediately (both
//! clones are dropped in short order), while a worker stuck in an
//! uninterruptible syscall keeps its permit reserved for as long as it is
//! actually still running, however late that thread eventually exits. See
//! ARCHITECTURE.md's known-issues entry and the upstream pattern this
//! mirrors: [runner hashFiles timeout issue
//! #1840](https://github.com/actions/runner/issues/1840).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::HashFilesError;

const MAX_LIVE_WORKERS: usize = 8;

static LIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// One call's reserved slot in the global live-worker cap, held by the
/// caller until it stops waiting and by the worker thread for its entire
/// lifetime.
pub(super) struct StrandGuard;

impl StrandGuard {
    /// Atomically reserves one live-worker slot, failing fast if the fixed
    /// cap is already reserved by other calls — whether those workers are
    /// still computing normally or are already stranded makes no
    /// difference; either way a new worker thread must not be started.
    pub(super) fn try_acquire() -> Result<Arc<Self>, HashFilesError> {
        LIVE_WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < MAX_LIVE_WORKERS).then_some(current + 1)
            })
            .map_err(|_| HashFilesError::StrandedWorkerLimit {
                max: MAX_LIVE_WORKERS,
            })?;
        Ok(Arc::new(Self))
    }
}

impl Drop for StrandGuard {
    fn drop(&mut self) {
        // Runs exactly once per reservation, when the last `Arc<StrandGuard>`
        // clone (always, in the end, the worker thread's own) is dropped —
        // see the module doc for why this needs no separate "was it
        // stranded" bookkeeping.
        LIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}
