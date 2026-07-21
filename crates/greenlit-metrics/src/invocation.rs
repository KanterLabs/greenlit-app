//! Span-timing layer: aggregate named stage durations for one invocation.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::Instrument;

use crate::record::{InvocationRecord, SCHEMA_VERSION, StageDuration};

/// Converts a [`Duration`] to fractional milliseconds for the NDJSON record.
fn duration_to_millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Milliseconds since the Unix epoch, for [`InvocationRecord::started_at_unix_ms`].
///
/// Falls back to `0` if the system clock is set before the epoch (it never
/// legitimately is on hosts Greenlit targets); metrics timestamps are
/// diagnostic, not correctness-critical, so a defensive fallback is
/// preferable to a panic here.
fn unix_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

/// Records the stage-by-stage wall-clock timing of a single `litci plan` or
/// `litci run` invocation.
///
/// Construct one with [`Invocation::start`] at the top of the command, time
/// each pipeline stage with [`time_stage`](Invocation::time_stage) or
/// [`time_stage_async`](Invocation::time_stage_async), then call
/// [`finish`](Invocation::finish) to obtain the completed [`InvocationRecord`]
/// ready for [`crate::MetricsStore::append`].
///
/// Cloning is cheap (an `Arc` clone) and every clone shares the same stage
/// list, so an `Invocation` can be passed down into concurrent work in later
/// phases without any change to this crate.
#[derive(Debug, Clone)]
pub struct Invocation {
    command: String,
    started_at: Instant,
    started_at_unix_ms: u128,
    stages: Arc<Mutex<Vec<StageDuration>>>,
}

impl Invocation {
    /// Begins timing a new invocation of the named CLI (sub)command (e.g.
    /// `"plan"` or `"run"`).
    pub fn start(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            started_at: Instant::now(),
            started_at_unix_ms: unix_millis_now(),
            stages: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Runs `f` inside a named `tracing` span and records its elapsed wall
    /// time, returning `f`'s result.
    ///
    /// The span is named `"stage"` and carries a `stage` field equal to
    /// `stage_name`. The closure may contain multiple statements and use `?`
    /// normally. Its duration is recorded even if the closure unwinds.
    pub fn time_stage<T>(&self, stage_name: &'static str, f: impl FnOnce() -> T) -> T {
        let _timer = StageTimer::start(stage_name, &self.stages);
        let span = tracing::info_span!("stage", stage = stage_name);
        span.in_scope(f)
    }

    /// Awaits `future` inside a named `tracing` span and records its elapsed
    /// wall time, returning the future's output.
    ///
    /// The returned future remains `Send` whenever the supplied future and
    /// its output are `Send`, so callers may use this method in work spawned
    /// on Tokio's multi-threaded runtime. Cancellation also records the time
    /// elapsed before the timed future was dropped.
    pub async fn time_stage_async<F>(&self, stage_name: &'static str, future: F) -> F::Output
    where
        F: Future,
    {
        let _timer = StageTimer::start(stage_name, &self.stages);
        let span = tracing::info_span!("stage", stage = stage_name);
        // `tracing` documents that an `EnteredSpan` must not be held across
        // `.await`: it can produce incorrect traces and makes the enclosing
        // future non-Send. `Instrument` instead enters the span only while
        // this future is being polled, so no entered guard crosses a
        // suspension point.
        future.instrument(span).await
    }

    /// Records a stage duration that the caller already measured elsewhere
    /// (for example, timing supplied by an external boundary this crate has
    /// no visibility into), without opening a span of its own.
    ///
    /// Prefer [`time_stage`](Self::time_stage) or
    /// [`time_stage_async`](Self::time_stage_async) when this crate is timing
    /// the work itself; this exists for the "record a duration I already
    /// have" half of this crate's contract.
    pub fn record_stage_duration(&self, stage_name: impl Into<String>, elapsed: Duration) {
        push_stage(&self.stages, stage_name.into(), elapsed);
    }

    /// Consumes the invocation, producing the completed [`InvocationRecord`]
    /// ready to append to a [`crate::MetricsStore`].
    ///
    /// Total duration is the wall-clock time from [`start`](Self::start) to
    /// this call, not the sum of stage durations — the two may legitimately
    /// differ (untimed glue work between stages, or, in later phases,
    /// overlapping concurrent stages).
    pub fn finish(self) -> InvocationRecord {
        let total_duration_ms = duration_to_millis(self.started_at.elapsed());
        // A poisoned mutex (a prior guard's Drop panicked mid-lock, which
        // cannot happen in this crate's own code but is defensive against
        // future changes) degrades to "no stages recorded" rather than
        // propagating a panic out of `finish`.
        let stages = self
            .stages
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        InvocationRecord {
            schema_version: SCHEMA_VERSION,
            command: self.command,
            started_at_unix_ms: self.started_at_unix_ms,
            total_duration_ms,
            stages,
        }
    }
}

fn push_stage(sink: &Arc<Mutex<Vec<StageDuration>>>, name: String, elapsed: Duration) {
    let duration_ms = duration_to_millis(elapsed);
    // A poisoned mutex is dropped silently rather than panicking: a timing
    // guard's `Drop` must never panic (double-panic during unwind aborts the
    // process), and losing one stage's timing is preferable to that.
    if let Ok(mut stages) = sink.lock() {
        stages.push(StageDuration { name, duration_ms });
    }
}

struct StageTimer {
    stage_name: &'static str,
    start: Instant,
    sink: Arc<Mutex<Vec<StageDuration>>>,
}

impl StageTimer {
    fn start(stage_name: &'static str, sink: &Arc<Mutex<Vec<StageDuration>>>) -> Self {
        Self {
            stage_name,
            start: Instant::now(),
            sink: Arc::clone(sink),
        }
    }
}

impl Drop for StageTimer {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        push_stage(&self.sink, self.stage_name.to_string(), elapsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::ready;
    use std::task::{Context, Poll, Waker};
    use std::thread::sleep;

    #[test]
    fn time_stage_records_one_duration_per_call() {
        let invocation = Invocation::start("plan");
        invocation.time_stage("parse", || sleep(Duration::from_millis(5)));
        invocation.time_stage("eval", || sleep(Duration::from_millis(5)));

        let record = invocation.finish();
        assert_eq!(record.command, "plan");
        assert_eq!(record.stages.len(), 2);
        assert_eq!(record.stages[0].name, "parse");
        assert_eq!(record.stages[1].name, "eval");
        // `sleep` guarantees *at least* the requested duration.
        assert!(record.stages[0].duration_ms >= 5.0);
        assert!(record.stages[1].duration_ms >= 5.0);
        assert!(record.total_duration_ms >= record.stages[0].duration_ms);
    }

    #[test]
    fn time_stage_async_is_send_and_records_completed_work() {
        let invocation = Invocation::start("run");
        let mut timed = Box::pin(invocation.time_stage_async("plan", ready(42)));

        fn assert_send<T: Send>(_: &T) {}
        assert_send(&timed);

        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(timed.as_mut().poll(&mut context), Poll::Ready(42));
        drop(timed);

        let record = invocation.finish();
        assert_eq!(record.stages.len(), 1);
        assert_eq!(record.stages[0].name, "plan");
        assert!(record.stages[0].duration_ms.is_finite());
        assert!(record.stages[0].duration_ms >= 0.0);
    }

    #[test]
    fn record_stage_duration_appends_a_precomputed_measurement() {
        let invocation = Invocation::start("run");
        invocation.record_stage_duration("external", Duration::from_millis(7));

        let record = invocation.finish();
        assert_eq!(record.stages.len(), 1);
        assert_eq!(record.stages[0].name, "external");
        assert!(record.stages[0].duration_ms >= 7.0);
    }

    #[test]
    fn schema_version_is_stamped_on_every_record() {
        let record = Invocation::start("plan").finish();
        assert_eq!(record.schema_version, SCHEMA_VERSION);
    }
}
