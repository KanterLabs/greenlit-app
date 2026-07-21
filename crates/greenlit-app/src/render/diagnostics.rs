//! stderr-only output: plan-time lints and per-stage timing. Never written
//! to stdout in either output mode -- stdout is reserved for the plan
//! itself (the tree in human mode, the JSON document with `--json`).

use std::io::Write;

use greenlit_engine::Lint;
use greenlit_metrics::InvocationRecord;

/// Renders every plan-time [`Lint`] as one `warning: file:line:col: message`
/// line.
pub(crate) fn render_lints(lints: &[Lint], out: &mut impl Write) -> std::io::Result<()> {
    for lint in lints {
        writeln!(out, "warning: {}: {}", lint.span, lint.message)?;
    }
    Ok(())
}

/// Renders one invocation's stage-by-stage timing breakdown as a small
/// table (`AGENTS.md` Metrics section: "The end-of-run table always shows
/// the stage breakdown").
pub(crate) fn render_timings(
    record: &InvocationRecord,
    out: &mut impl Write,
) -> std::io::Result<()> {
    writeln!(out, "stage timings ({}):", record.command)?;
    for stage in &record.stages {
        writeln!(out, "  {:<10} {:>9.2} ms", stage.name, stage.duration_ms)?;
    }
    writeln!(
        out,
        "  {:<10} {:>9.2} ms",
        "total", record.total_duration_ms
    )?;
    Ok(())
}
