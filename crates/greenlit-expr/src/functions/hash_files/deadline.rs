//! Runner-compatible wall-clock deadline for `hashFiles()` work.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use super::HashFilesError;

const HASH_FILES_TIMEOUT_SECONDS: u64 = 120;

/// Supplemental elapsed-time source for deterministic `hashFiles()` deadline
/// verification.
///
/// Greenlit always compares this value with the real monotonic time elapsed
/// since the operation began and uses the greater duration. Consequently, an
/// implementation can only accelerate the fixed 120-second deadline; it
/// cannot delay or disable that security boundary. The caller supervisor and
/// filesystem worker may query the clock concurrently.
pub trait HashFilesClock: std::fmt::Debug + Send + Sync {
    /// Reports the elapsed duration to consider in addition to real monotonic
    /// time since `started`.
    fn elapsed(&self, started: Instant) -> Duration;
}

#[derive(Debug)]
pub(crate) struct SystemHashFilesClock;

impl HashFilesClock for SystemHashFilesClock {
    fn elapsed(&self, started: Instant) -> Duration {
        started.elapsed()
    }
}

/// One fixed deadline shared by glob traversal and file hashing.
#[derive(Debug)]
pub(super) struct HashFilesDeadline<'a> {
    started: Instant,
    escaped_patterns: String,
    supplemental_clock: &'a dyn HashFilesClock,
}

impl<'a> HashFilesDeadline<'a> {
    pub(super) fn new(
        patterns: &[String],
        supplemental_clock: &'a dyn HashFilesClock,
        started: Instant,
    ) -> Self {
        Self {
            started,
            // `ExpressionUtility.StringEscape` doubles apostrophes before
            // the runner places the joined patterns in its timeout error.
            // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/ExpressionUtility.cs
            escaped_patterns: patterns.join(", ").replace('\'', "''"),
            supplemental_clock,
        }
    }

    pub(super) fn check(&self) -> Result<(), HashFilesError> {
        if self.expired() {
            Err(self.timeout_error())
        } else {
            Ok(())
        }
    }

    /// Returns a bounded wait duration for the caller-side supervisor.
    /// Recomputing this after every wait observes both monotonic wall time and
    /// an acceleration-only injected clock while keeping the latter unable to
    /// extend the real deadline.
    pub(super) fn wait_slice(&self, maximum: Duration) -> Result<Duration, HashFilesError> {
        let elapsed = self.elapsed();
        let timeout = Duration::from_secs(HASH_FILES_TIMEOUT_SECONDS);
        if elapsed >= timeout {
            return Err(self.timeout_error());
        }
        Ok((timeout - elapsed).min(maximum))
    }

    pub(super) fn check_io(&self) -> io::Result<()> {
        if self.expired() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "hashFiles deadline elapsed",
            ))
        } else {
            Ok(())
        }
    }

    pub(super) fn map_io(&self, path: &Path, source: io::Error) -> HashFilesError {
        if self.expired() {
            self.timeout_error()
        } else {
            HashFilesError::Io {
                path: path.display().to_string(),
                source,
            }
        }
    }

    fn expired(&self) -> bool {
        self.elapsed() >= Duration::from_secs(HASH_FILES_TIMEOUT_SECONDS)
    }

    fn elapsed(&self) -> Duration {
        self.started
            .elapsed()
            .max(self.supplemental_clock.elapsed(self.started))
    }

    fn timeout_error(&self) -> HashFilesError {
        HashFilesError::Timeout {
            patterns: self.escaped_patterns.clone(),
            seconds: HASH_FILES_TIMEOUT_SECONDS,
        }
    }
}
