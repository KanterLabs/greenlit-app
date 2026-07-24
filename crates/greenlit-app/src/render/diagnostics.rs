//! stderr-only output: plan-time lints and per-stage timing. Never written
//! to stdout in either output mode -- stdout is reserved for the plan
//! itself (the tree in human mode, the JSON document with `--json`).

use std::io::Write;

use greenlit_engine::Lint;
use greenlit_metrics::InvocationRecord;

/// Renders every plan-time [`Lint`] as one `warning: file:line:col: message`
/// line.
pub(crate) fn render_lints(lints: &[Lint], out: &mut impl Write) -> std::io::Result<()> {
    super::terminal::render_sanitized(out, |buffer| {
        for lint in lints {
            writeln!(
                buffer,
                "warning: {}: {}",
                super::terminal::inline_escape(&lint.span.to_string()),
                super::terminal::inline_escape(&lint.message)
            )?;
        }
        Ok(())
    })
}

/// Renders one invocation's stage-by-stage timing breakdown as a small
/// table (`AGENTS.md` Metrics section: "The end-of-run table always shows
/// the stage breakdown").
pub(crate) fn render_timings(
    record: &InvocationRecord,
    out: &mut impl Write,
) -> std::io::Result<()> {
    super::terminal::render_sanitized(out, |buffer| {
        writeln!(buffer, "stage timings ({}):", record.command)?;
        for stage in &record.stages {
            writeln!(buffer, "  {:<10} {:>9.2} ms", stage.name, stage.duration_ms)?;
        }
        writeln!(
            buffer,
            "  {:<10} {:>9.2} ms",
            "total", record.total_duration_ms
        )?;
        // Local-cache hit/miss counters (action source store, pinned
        // Node-runtime bundle cache) — empty for a run that never touched a
        // `uses:` step, so nothing prints then.
        for counter in &record.hit_miss {
            writeln!(
                buffer,
                "  {:<10} {} hit(s), {} miss(es)",
                counter.name, counter.hits, counter.misses
            )?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use greenlit_metrics::{HitMissCounter, StageDuration};

    #[test]
    fn hit_miss_counters_render_one_line_each_and_are_absent_when_empty() {
        let mut record = InvocationRecord {
            schema_version: 1,
            command: "run".to_string(),
            started_at_unix_ms: 0,
            total_duration_ms: 12.0,
            stages: vec![StageDuration {
                name: "plan".to_string(),
                duration_ms: 1.0,
            }],
            steps: Vec::new(),
            hit_miss: Vec::new(),
        };

        let mut out = Vec::new();
        render_timings(&record, &mut out).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert!(
            !rendered.contains("hit(s)"),
            "no hit/miss line when nothing was looked up: {rendered}"
        );

        record.hit_miss = vec![
            HitMissCounter {
                name: "action-fetch".to_string(),
                hits: 3,
                misses: 1,
            },
            HitMissCounter {
                name: "action-runtime-fetch".to_string(),
                hits: 0,
                misses: 2,
            },
        ];
        let mut out = Vec::new();
        render_timings(&record, &mut out).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("action-fetch 3 hit(s), 1 miss(es)"));
        assert!(rendered.contains("action-runtime-fetch 0 hit(s), 2 miss(es)"));
    }
}
