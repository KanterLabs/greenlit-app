//! `litci stats`: render local invocation history and per-stage duration
//! trends, read-only (`PHASE-1-engine-core.md`: "no network access, and
//! must NOT append a new metrics record itself").

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use greenlit_metrics::{InvocationRecord, MetricsStore};

use crate::errors;

const RECENT_INVOCATION_LIMIT: usize = 20;

pub(crate) fn run() -> anyhow::Result<()> {
    // Read-only: `MetricsStore::append` is never called here
    // (`PHASE-1-engine-core.md`, `AGENTS.md` Metrics section: "Read-only
    // reporting commands such as `stats` do not append records").
    let store = MetricsStore::open_default().map_err(|error| errors::metrics_error(&error))?;
    let records = store
        .read_recent(RECENT_INVOCATION_LIMIT)
        .map_err(|error| errors::metrics_error(&error))?;

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    crate::render::terminal::render_sanitized(&mut handle, |buffer| {
        render(&records, store.path(), buffer)
    })
    .map_err(|error| {
        anyhow::anyhow!(
            "could not write metrics history to stdout: {error}\n  fix: ensure stdout is writable, then retry"
        )
    })
}

fn render(
    records: &[InvocationRecord],
    store_path: &Path,
    out: &mut impl Write,
) -> std::io::Result<()> {
    if records.is_empty() {
        let safe_store_path = crate::render::terminal::inline_escape(&store_path.to_string_lossy());
        writeln!(
            out,
            "no invocation history yet at {} -- run `litci plan` to record one",
            safe_store_path
        )?;
        return Ok(());
    }

    writeln!(
        out,
        "recent invocations (up to {RECENT_INVOCATION_LIMIT}, {} shown):",
        records.len()
    )?;
    for record in records {
        let stages = record
            .stages
            .iter()
            .map(|s| {
                format!(
                    "{}={:.2}ms",
                    crate::render::terminal::inline_escape(&s.name),
                    s.duration_ms
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let command = crate::render::terminal::inline_escape(&record.command);
        writeln!(
            out,
            "  t={} {:<6} total={:.2}ms stages=[{stages}]",
            record.started_at_unix_ms, command, record.total_duration_ms
        )?;
    }

    writeln!(out)?;
    writeln!(
        out,
        "stage trends (avg / min / max over {} invocations):",
        records.len()
    )?;
    for (name, durations) in stage_trends(records) {
        let (avg, min, max) = summarize(&durations);
        let name = crate::render::terminal::inline_escape(&name);
        writeln!(
            out,
            "  {name:<10} avg={avg:.2}ms min={min:.2}ms max={max:.2}ms n={}",
            durations.len()
        )?;
    }

    Ok(())
}

fn stage_trends(records: &[InvocationRecord]) -> Vec<(String, Vec<f64>)> {
    let mut by_name: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for record in records {
        for stage in &record.stages {
            by_name
                .entry(stage.name.clone())
                .or_default()
                .push(stage.duration_ms);
        }
    }
    by_name.into_iter().collect()
}

/// `durations` is only ever built from at least one recorded sample (see
/// [`stage_trends`], which only inserts a name once a stage with that name
/// exists), so the fold-based min/max below never operate on an empty
/// slice.
fn summarize(durations: &[f64]) -> (f64, f64, f64) {
    let sum: f64 = durations.iter().sum();
    let avg = sum / durations.len() as f64;
    let min = durations.iter().copied().fold(f64::INFINITY, f64::min);
    let max = durations.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (avg, min, max)
}
