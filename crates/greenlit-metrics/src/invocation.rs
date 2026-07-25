//! Span-timing layer: aggregate named stage durations for one invocation.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::Instrument;
use tracing::field::{Field, Visit};
use tracing::{Id, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

use crate::record::{
    HitMissCounter, InvocationRecord, SCHEMA_VERSION, StageDuration, StepDuration,
};

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
/// run the pipeline inside [`with_timing_subscriber`](Invocation::with_timing_subscriber),
/// instrument each stage with [`time_stage`](Invocation::time_stage) or
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
    steps: Arc<Mutex<Vec<StepDuration>>>,
    hit_miss: Arc<Mutex<Vec<HitMissCounter>>>,
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
            steps: Arc::new(Mutex::new(Vec::new())),
            hit_miss: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Runs `f` with a local tracing subscriber that aggregates spans whose
    /// target is `greenlit_metrics::timed_stage` and whose `stage` field is
    /// a string. This is scoped to the closure and never installs global
    /// logging or telemetry.
    pub fn with_timing_subscriber<T>(&self, f: impl FnOnce() -> T) -> T {
        let subscriber = Registry::default().with(TimingLayer {
            sink: Arc::clone(&self.stages),
        });
        tracing::subscriber::with_default(subscriber, f)
    }

    /// Runs `f` inside a named `tracing` span and records its elapsed wall
    /// time, returning `f`'s result.
    ///
    /// The span is named `"stage"` and carries a `stage` field equal to
    /// `stage_name`. The closure may contain multiple statements and use `?`
    /// normally. Its duration is recorded even if the closure unwinds.
    pub fn time_stage<T>(&self, stage_name: &'static str, f: impl FnOnce() -> T) -> T {
        let span = tracing::info_span!(
            target: "greenlit_metrics::timed_stage",
            "greenlit_stage",
            stage = stage_name
        );
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
        let span = tracing::info_span!(
            target: "greenlit_metrics::timed_stage",
            "greenlit_stage",
            stage = stage_name
        );
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

    /// Records one completed workflow step duration.
    pub fn record_step_duration(
        &self,
        job: impl Into<String>,
        step: impl Into<String>,
        elapsed: Duration,
    ) {
        if let Ok(mut steps) = self.steps.lock() {
            steps.push(StepDuration {
                job: job.into(),
                step: step.into(),
                duration_ms: duration_to_millis(elapsed),
            });
        }
    }

    /// Increments a named hit/miss counter.
    pub fn record_lookup(&self, name: impl Into<String>, hit: bool) {
        self.record_lookup_bytes(name, hit, 0);
    }

    /// Increments a named counter and adds to the bytes it has moved.
    ///
    /// Separate from [`Invocation::record_lookup`] because most counters have
    /// no meaningful byte count — an action fetch either hit the store or did
    /// not — and defaulting to zero keeps those call sites unchanged rather
    /// than making every one of them pass a `0`.
    pub fn record_lookup_bytes(&self, name: impl Into<String>, hit: bool, bytes: u64) {
        let name = name.into();
        if let Ok(mut counters) = self.hit_miss.lock() {
            if let Some(counter) = counters.iter_mut().find(|counter| counter.name == name) {
                if hit {
                    counter.hits = counter.hits.saturating_add(1);
                } else {
                    counter.misses = counter.misses.saturating_add(1);
                }
                counter.bytes = counter.bytes.saturating_add(bytes);
            } else {
                counters.push(HitMissCounter {
                    name,
                    hits: u64::from(hit),
                    misses: u64::from(!hit),
                    bytes,
                });
            }
        }
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
        let steps = self
            .steps
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let hit_miss = self
            .hit_miss
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        InvocationRecord {
            schema_version: SCHEMA_VERSION,
            command: self.command,
            started_at_unix_ms: self.started_at_unix_ms,
            total_duration_ms,
            stages,
            steps,
            hit_miss,
        }
    }
}

fn push_stage(sink: &Arc<Mutex<Vec<StageDuration>>>, name: String, elapsed: Duration) {
    let duration_ms = duration_to_millis(elapsed);
    // A poisoned mutex is dropped silently rather than panicking: a timing
    // guard's `Drop` must never panic (double-panic during unwind aborts the
    // process), and losing one stage's timing is preferable to that.
    if let Ok(mut stages) = sink.lock() {
        if let Some(existing) = stages.iter_mut().find(|stage| stage.name == name) {
            existing.duration_ms += duration_ms;
        } else {
            stages.push(StageDuration { name, duration_ms });
        }
    }
}

#[derive(Debug)]
struct TimingLayer {
    sink: Arc<Mutex<Vec<StageDuration>>>,
}

#[derive(Debug)]
struct TracedStage {
    name: String,
    started_at: Instant,
}

#[derive(Default)]
struct StageNameVisitor {
    name: Option<String>,
}

impl Visit for StageNameVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "stage" {
            self.name = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "stage" && self.name.is_none() {
            self.name = Some(format!("{value:?}").trim_matches('"').to_string());
        }
    }
}

impl<S> Layer<S> for TimingLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &Id,
        ctx: LayerContext<'_, S>,
    ) {
        if attrs.metadata().target() != "greenlit_metrics::timed_stage" {
            return;
        }
        let mut visitor = StageNameVisitor::default();
        attrs.record(&mut visitor);
        let Some(name) = visitor.name else { return };
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(TracedStage {
                name,
                started_at: Instant::now(),
            });
        }
    }

    fn on_close(&self, id: Id, ctx: LayerContext<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(stage) = span.extensions_mut().remove::<TracedStage>() else {
            return;
        };
        push_stage(&self.sink, stage.name, stage.started_at.elapsed());
    }
}
