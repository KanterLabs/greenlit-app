//! Durable run-event journal and terminal projections.
//!
//! Execution state arrives through typed runtime ports. This module records
//! those transitions before projecting them as human text, so repository
//! output can never impersonate Greenlit lifecycle events.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use greenlit_runtime::JobScope;

use crate::cli::{ColorArg, LogModeArg, RunFormatArg};

mod progress;
mod schema;
mod sinks;
mod terminal;

pub(crate) use schema::{RunEvent, RunEventRecord};

const FAILURE_TAIL_LINES: usize = 200;
const FAILURE_TAIL_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
struct ActiveStep {
    event_id: String,
}

#[derive(Debug, Default)]
struct TailBuffer {
    lines: VecDeque<String>,
    bytes: usize,
}

#[derive(Debug)]
struct LineBuffer {
    scope: JobScope,
    bytes: Vec<u8>,
}

impl TailBuffer {
    fn push(&mut self, line: String) {
        self.bytes = self.bytes.saturating_add(line.len());
        self.lines.push_back(line);
        while self.lines.len() > FAILURE_TAIL_LINES || self.bytes > FAILURE_TAIL_BYTES {
            let Some(removed) = self.lines.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(removed.len());
        }
    }
}

struct State {
    run_id: String,
    started: Instant,
    sequence: u64,
    journal: File,
    output: Box<dyn Write + Send>,
    format: RunFormatArg,
    log_mode: LogModeArg,
    styled: bool,
    active_steps: HashMap<String, ActiveStep>,
    line_buffers: HashMap<String, LineBuffer>,
    tails: HashMap<(String, String), TailBuffer>,
    failure: Option<String>,
    terminal_written: bool,
    result_publication_abandoned: Arc<AtomicBool>,
}

/// Cloneable recorder handle used simultaneously as all runtime output ports.
#[derive(Clone)]
pub(crate) struct RunEventRecorder {
    state: Arc<Mutex<State>>,
}

impl RunEventRecorder {
    pub(crate) fn create(
        directory: &Path,
        run_id: &str,
        format: RunFormatArg,
        log_mode: LogModeArg,
        color: ColorArg,
        result_publication_abandoned: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let recorder = Self::create_with_output(
            directory,
            run_id,
            format,
            log_mode,
            color,
            Box::new(io::stdout()),
            Arc::clone(&result_publication_abandoned),
        );
        if recorder.is_err() {
            result_publication_abandoned.store(true, Ordering::Release);
        }
        recorder
    }

    fn create_with_output(
        directory: &Path,
        run_id: &str,
        format: RunFormatArg,
        log_mode: LogModeArg,
        color: ColorArg,
        output: Box<dyn Write + Send>,
        result_publication_abandoned: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let path = directory.join("events.ndjson");
        let journal = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| {
                anyhow::anyhow!(
                    "could not create run event journal {}: {error}\n  fix: ensure the run directory is writable, then retry",
                    path.display()
                )
            })?;
        let styled = match color {
            ColorArg::Always => true,
            ColorArg::Never => false,
            ColorArg::Auto => io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        };
        let recorder = Self {
            state: Arc::new(Mutex::new(State {
                run_id: run_id.to_string(),
                started: Instant::now(),
                sequence: 1,
                journal,
                output,
                format,
                log_mode,
                styled,
                active_steps: HashMap::new(),
                line_buffers: HashMap::new(),
                tails: HashMap::new(),
                failure: None,
                terminal_written: false,
                result_publication_abandoned,
            })),
        };
        recorder.record(RunEvent::RunStarted)?;
        Ok(recorder)
    }

    pub(crate) fn finish(
        &self,
        conclusion: impl Into<String>,
        compatibility: impl Into<String>,
        assurance: impl Into<String>,
    ) -> anyhow::Result<()> {
        self.flush_partial_lines()?;
        let run_id = self.lock().run_id.clone();
        self.record(RunEvent::RunFinished {
            conclusion: conclusion.into(),
            compatibility: compatibility.into(),
            assurance: assurance.into(),
            evidence: run_id,
        })?;
        let mut state = self.lock();
        state.terminal_written = true;
        if let Err(error) = state.journal.sync_all() {
            state
                .result_publication_abandoned
                .store(true, Ordering::Release);
            return Err(anyhow::anyhow!(
                "could not make the completed run event journal durable: {error}\n  fix: ensure HOME has free space, then retry"
            ));
        }
        if let Err(error) = state.output.flush() {
            state
                .result_publication_abandoned
                .store(true, Ordering::Release);
            return Err(anyhow::anyhow!(
                "could not flush run output: {error}\n  fix: ensure stdout is writable"
            ));
        }
        if let Some(message) = state.failure.take() {
            state
                .result_publication_abandoned
                .store(true, Ordering::Release);
            anyhow::bail!("{message}");
        }
        Ok(())
    }

    pub(crate) fn flush_pending_logs(&self) -> anyhow::Result<()> {
        self.flush_partial_lines()
    }

    pub(crate) fn verify_durable(&self) -> anyhow::Result<()> {
        let mut state = self.lock();
        if let Err(error) = state.journal.sync_all() {
            state
                .result_publication_abandoned
                .store(true, Ordering::Release);
            return Err(anyhow::anyhow!(
                "could not make the run event journal durable: {error}\n  fix: ensure HOME has free space, then retry"
            ));
        }
        if let Some(message) = state.failure.take() {
            state
                .result_publication_abandoned
                .store(true, Ordering::Release);
            anyhow::bail!("{message}");
        }
        Ok(())
    }

    pub(crate) fn preparation_finished(
        &self,
        phase: &str,
        detail: Option<String>,
    ) -> anyhow::Result<()> {
        self.record(preparation(phase, "finished", detail))
    }

    pub(crate) fn cache_summary(&self, store: &str, hits: u64, misses: u64) -> anyhow::Result<()> {
        self.record(RunEvent::CacheSummary {
            store: store.to_string(),
            hits,
            misses,
        })
    }

    pub(crate) fn compatibility_findings(
        &self,
        report: &greenlit_engine::SupportReport,
    ) -> anyhow::Result<()> {
        for finding in &report.findings {
            self.record(RunEvent::CompatibilityFinding {
                code: finding.code.clone(),
                disposition: finding.disposition,
                scope: finding.scope.clone(),
                reason: finding.reason.clone(),
            })?;
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn record(&self, event: RunEvent) -> anyhow::Result<()> {
        let mut state = self.lock();
        write_record(&mut state, event).map_err(|error| {
            anyhow::anyhow!(
                "could not persist the run event journal: {error}\n  fix: ensure HOME has free space, then retry"
            )
        })
    }

    fn flush_partial_lines(&self) -> anyhow::Result<()> {
        let pending = {
            let mut state = self.lock();
            std::mem::take(&mut state.line_buffers)
        };
        for (_, line_buffer) in pending {
            if line_buffer.bytes.is_empty() {
                continue;
            }
            self.record_log_line(
                &line_buffer.scope,
                String::from_utf8_lossy(&line_buffer.bytes).into_owned(),
                true,
            )?;
        }
        Ok(())
    }

    fn flush_scope_line(&self, scope: &JobScope) -> anyhow::Result<()> {
        let pending = self.lock().line_buffers.remove(&scope.instance_id);
        if let Some(line_buffer) = pending
            && !line_buffer.bytes.is_empty()
        {
            self.record_log_line(
                &line_buffer.scope,
                String::from_utf8_lossy(&line_buffer.bytes).into_owned(),
                true,
            )
            .map_err(|error| anyhow::anyhow!(error))?;
        }
        Ok(())
    }

    fn record_log_line(&self, scope: &JobScope, text: String, partial: bool) -> io::Result<()> {
        let mut state = self.lock();
        let event_id = state
            .active_steps
            .get(&scope.instance_id)
            .map(|active| active.event_id.clone());
        let event = RunEvent::Log {
            job_id: scope.job_id.clone(),
            instance_id: scope.instance_id.clone(),
            step_event_id: event_id.clone(),
            text: text.clone(),
            partial,
        };
        write_record(&mut state, event)?;
        if let Some(event_id) = event_id {
            state
                .tails
                .entry((scope.instance_id.clone(), event_id))
                .or_default()
                .push(text.clone());
        }
        if state.format == RunFormatArg::Plain && state.log_mode == LogModeArg::Full {
            let escaped = crate::render::terminal::inline_escape(&text);
            writeln!(state.output, "    {escaped}")?;
        }
        Ok(())
    }

    fn remember_failure(&self, error: io::Error) {
        let mut state = self.lock();
        state
            .result_publication_abandoned
            .store(true, Ordering::Release);
        if state.failure.is_none() {
            state.failure = Some(format!(
                "could not record run output: {error}\n  fix: ensure stdout and HOME are writable"
            ));
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if self.terminal_written {
            return;
        }
        let event = RunEvent::RunFinished {
            conclusion: "Aborted".to_string(),
            compatibility: "Degraded".to_string(),
            assurance: "None".to_string(),
            evidence: self.run_id.clone(),
        };
        self.terminal_written = true;
        let record = write_record(self, event);
        let sync = self.journal.sync_all();
        if record.is_err() || sync.is_err() {
            self.result_publication_abandoned
                .store(true, Ordering::Release);
        }
    }
}

fn write_record(state: &mut State, event: RunEvent) -> io::Result<()> {
    let result = write_record_inner(state, event);
    if result.is_err() {
        state
            .result_publication_abandoned
            .store(true, Ordering::Release);
    }
    result
}

fn write_record_inner(state: &mut State, event: RunEvent) -> io::Result<()> {
    let timestamp_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let elapsed_ms = state
        .started
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let record = RunEventRecord {
        schema_version: schema::VERSION,
        sequence: state.sequence,
        timestamp_unix_ms,
        elapsed_ms,
        run_id: state.run_id.clone(),
        event,
    };
    state.sequence = state.sequence.saturating_add(1);
    serde_json::to_writer(&mut state.journal, &record).map_err(io::Error::other)?;
    state.journal.write_all(b"\n")?;
    state.journal.flush()?;
    if state.format == RunFormatArg::Jsonl {
        serde_json::to_writer(&mut state.output, &record).map_err(io::Error::other)?;
        state.output.write_all(b"\n")?;
        state.output.flush()?;
    } else {
        terminal::render(state, &record)?;
    }
    Ok(())
}

fn preparation(phase: &str, state: &str, detail: Option<String>) -> RunEvent {
    RunEvent::Preparation {
        phase: phase.to_string(),
        state: state.to_string(),
        detail,
        current_bytes: None,
        total_bytes: None,
        cache_hit: None,
    }
}
