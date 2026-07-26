//! Plain terminal projection of persisted event records.

use std::io::{self, Write};

use super::{RunEvent, RunEventRecord, State};

pub(super) fn render(state: &mut State, record: &RunEventRecord) -> io::Result<()> {
    match &record.event {
        RunEvent::RunStarted => writeln!(state.output, "Greenlit  {}", state.run_id)?,
        RunEvent::Preparation {
            phase,
            state: event_state,
            detail,
            cache_hit,
            ..
        } if event_state == "finished" || event_state == "ready" || event_state == "resolved" => {
            let marker = if cache_hit == &Some(true) {
                "cached"
            } else {
                event_state
            };
            let detail = detail
                .as_deref()
                .map(crate::render::terminal::inline_escape)
                .unwrap_or_default();
            let phase = crate::render::terminal::inline_escape(phase);
            if detail.is_empty() {
                writeln!(state.output, "  {phase:<18} {marker}")?;
            } else {
                writeln!(state.output, "  {phase:<18} {marker}  {detail}")?;
            }
        }
        RunEvent::JobStarted { display, .. } => writeln!(
            state.output,
            "\njob {}",
            crate::render::terminal::inline_escape(display)
        )?,
        RunEvent::JobSkipped {
            display, reason, ..
        } => writeln!(
            state.output,
            "\njob {}  - skipped ({})",
            crate::render::terminal::inline_escape(display),
            crate::render::terminal::inline_escape(reason)
        )?,
        RunEvent::JobFinished {
            display,
            conclusion,
            duration_ms,
            ..
        } if conclusion == "cancelled" => writeln!(
            state.output,
            "\njob {}  CANCEL  {}",
            crate::render::terminal::inline_escape(display),
            format_duration(*duration_ms)
        )?,
        RunEvent::StepSkipped { label, reason, .. } => writeln!(
            state.output,
            "  - {}  {}",
            crate::render::terminal::inline_escape(label),
            crate::render::terminal::inline_escape(reason)
        )?,
        RunEvent::StepFinished {
            instance_id,
            event_id,
            label,
            conclusion,
            duration_ms,
            ..
        } => render_step(
            state,
            instance_id,
            event_id,
            label,
            conclusion,
            *duration_ms,
        )?,
        RunEvent::CacheSummary {
            store,
            hits,
            misses,
        } => writeln!(
            state.output,
            "  {store:<18} {hits} hit(s), {misses} miss(es)"
        )?,
        RunEvent::RunFinished {
            conclusion,
            compatibility,
            assurance,
            evidence,
        } => render_result(state, conclusion, compatibility, assurance, evidence)?,
        _ => {}
    }
    state.output.flush()
}

fn render_step(
    state: &mut State,
    instance_id: &str,
    event_id: &str,
    label: &str,
    conclusion: &str,
    duration_ms: u64,
) -> io::Result<()> {
    let (symbol, color) = match conclusion {
        "success" => (if state.styled { "✓" } else { "OK" }, "32"),
        "failure" => (if state.styled { "✗" } else { "FAIL" }, "31"),
        "cancelled" => (if state.styled { "⊘" } else { "CANCEL" }, "33"),
        _ => (if state.styled { "–" } else { "SKIP" }, "33"),
    };
    let symbol = if state.styled {
        format!("\u{1b}[{color}m{symbol}\u{1b}[0m")
    } else {
        symbol.to_string()
    };
    writeln!(
        state.output,
        "  {symbol:<6} {}  {}",
        crate::render::terminal::inline_escape(label),
        format_duration(duration_ms)
    )?;
    if conclusion == "failure" {
        if let Some(tail) = state
            .tails
            .get(&(instance_id.to_string(), event_id.to_string()))
        {
            for line in &tail.lines {
                writeln!(
                    state.output,
                    "      {}",
                    crate::render::terminal::inline_escape(line)
                )?;
            }
        }
        writeln!(
            state.output,
            "      full log: litci logs {} --job {} --step {}",
            state.run_id, instance_id, event_id
        )?;
    }
    Ok(())
}

fn render_result(
    state: &mut State,
    conclusion: &str,
    compatibility: &str,
    assurance: &str,
    evidence: &str,
) -> io::Result<()> {
    let label = if conclusion == "Passed" && compatibility == "Degraded" && state.styled {
        "Passed locally — degraded compatibility"
    } else if conclusion == "Passed" && compatibility == "Degraded" {
        "Passed locally - degraded compatibility"
    } else {
        conclusion
    };
    let symbol = if conclusion == "Passed" {
        if state.styled { "✓" } else { "OK" }
    } else if state.styled {
        "✗"
    } else {
        "FAIL"
    };
    writeln!(state.output, "\n{symbol} {label}")?;
    writeln!(
        state.output,
        "  evidence: {evidence} ({conclusion}/{compatibility}/{assurance})"
    )?;
    writeln!(state.output, "  logs:     litci logs {evidence}")
}

fn format_duration(duration_ms: u64) -> String {
    if duration_ms >= 1000 {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    } else {
        format!("{duration_ms}ms")
    }
}
