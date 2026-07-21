//! `litci stats`: render local invocation history and per-stage duration
//! trends, read-only (`PHASE-1-engine-core.md`: "no network access, and
//! must NOT append a new metrics record itself").

use std::collections::BTreeMap;

use greenlit_metrics::{InvocationRecord, MetricsStore};

pub(crate) fn run() -> anyhow::Result<()> {
    // Read-only: `MetricsStore::append` is never called here
    // (`PHASE-1-engine-core.md`, `AGENTS.md` Metrics section: "Read-only
    // reporting commands such as `stats` do not append records").
    let store = MetricsStore::open_default()?;
    let records = store.read_all()?;

    if records.is_empty() {
        println!(
            "no invocation history yet at {} -- run `litci plan` to record one",
            store.path().display()
        );
        return Ok(());
    }

    println!("recent invocations ({} total):", records.len());
    for record in &records {
        let stages = record
            .stages
            .iter()
            .map(|s| format!("{}={:.2}ms", s.name, s.duration_ms))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  t={} {:<6} total={:.2}ms stages=[{stages}]",
            record.started_at_unix_ms, record.command, record.total_duration_ms
        );
    }

    println!();
    println!(
        "stage trends (avg / min / max over {} invocations):",
        records.len()
    );
    for (name, durations) in stage_trends(&records) {
        let (avg, min, max) = summarize(&durations);
        println!(
            "  {name:<10} avg={avg:.2}ms min={min:.2}ms max={max:.2}ms n={}",
            durations.len()
        );
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
