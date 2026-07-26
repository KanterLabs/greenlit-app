//! Read-only replay of redacted log events from a persisted run journal.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::cli::{LogFormatArg, LogsArgs};
use crate::run_events::{RunEvent, RunEventRecord};

#[derive(Debug, Clone)]
struct StepIdentity {
    index: usize,
    step_id: Option<String>,
}

pub(crate) fn run(args: LogsArgs) -> anyhow::Result<()> {
    let runs = crate::inspect_cmd::runs_root()?;
    let (run_id, journal) = select_journal(&runs, args.run_id.as_deref())?;
    let mut offset = 0;
    let mut terminal = false;
    let mut steps = HashMap::new();
    let mut tail = VecDeque::new();
    let mut first_pass = true;
    let tail_limit = args
        .tail
        .map(|value| usize::try_from(value).unwrap_or(usize::MAX));

    loop {
        let bytes = fs::read(&journal).map_err(|error| {
            anyhow::anyhow!(
                "could not read logs for run '{run_id}': {error}\n  fix: use `litci doctor` to check the run journal"
            )
        })?;
        if offset > bytes.len() {
            anyhow::bail!(
                "run journal for '{run_id}' shrank while it was being read\n  fix: preserve the run directory and use `litci doctor`"
            );
        }
        let appended = &bytes[offset..];
        let complete_len = appended
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1);
        if !args.follow && complete_len != appended.len() {
            anyhow::bail!(
                "run journal for '{run_id}' ends with an incomplete event\n  fix: preserve the run directory and use `litci doctor`"
            );
        }
        for line in appended[..complete_len].split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let record = serde_json::from_slice::<RunEventRecord>(line).map_err(|error| {
                anyhow::anyhow!(
                    "run journal for '{run_id}' contains an invalid event: {error}\n  fix: preserve the run directory and use `litci doctor`"
                )
            })?;
            index_step(&record, &mut steps);
            terminal |= matches!(record.event, RunEvent::RunFinished { .. });
            if matches_filter(&record, &args, &steps) {
                let rendered = render_record(line, &record, args.format)?;
                if let Some(limit) = tail_limit
                    && first_pass
                {
                    tail.push_back(rendered);
                    while tail.len() > limit {
                        tail.pop_front();
                    }
                } else {
                    write_rendered(&rendered)?;
                }
            }
        }
        offset = offset.saturating_add(complete_len);
        if first_pass {
            for rendered in tail.drain(..) {
                write_rendered(&rendered)?;
            }
            first_pass = false;
        }
        if !args.follow || terminal {
            break;
        }
        thread::sleep(Duration::from_millis(150));
    }
    Ok(())
}

fn select_journal(runs: &Path, requested: Option<&str>) -> anyhow::Result<(String, PathBuf)> {
    if let Some(requested) = requested {
        let run_id = crate::inspect_cmd::validate_run_id(requested)?;
        let journal = runs.join(&run_id).join("events.ndjson");
        if !journal.is_file() {
            anyhow::bail!(
                "run '{run_id}' has no structured log journal\n  fix: choose a run created by this version of Greenlit"
            );
        }
        return Ok((run_id, journal));
    }
    let entries = fs::read_dir(runs).map_err(|error| {
        anyhow::anyhow!(
            "could not list run logs at {}: {error}\n  fix: run a workflow first, or make HOME readable",
            runs.display()
        )
    })?;
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| crate::inspect_cmd::validate_run_id(name).is_ok())
        .filter_map(|run_id| {
            let path = runs.join(&run_id).join("events.ndjson");
            path.is_file().then_some((run_id, path))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no structured run logs exist\n  fix: run `litci run` once with this version, then retry"
            )
        })
}

fn index_step(record: &RunEventRecord, steps: &mut HashMap<(String, String), StepIdentity>) {
    if let RunEvent::StepStarted {
        instance_id,
        event_id,
        index,
        step_id,
        ..
    } = &record.event
    {
        steps.insert(
            (instance_id.clone(), event_id.clone()),
            StepIdentity {
                index: *index,
                step_id: step_id.clone(),
            },
        );
    }
}

fn matches_filter(
    record: &RunEventRecord,
    args: &LogsArgs,
    steps: &HashMap<(String, String), StepIdentity>,
) -> bool {
    let RunEvent::Log {
        job_id,
        instance_id,
        step_event_id,
        ..
    } = &record.event
    else {
        return false;
    };
    if let Some(selected) = &args.job
        && selected != job_id
        && selected != instance_id
    {
        return false;
    }
    let Some(selected) = &args.step else {
        return true;
    };
    let Some(event_id) = step_event_id else {
        return false;
    };
    if selected == event_id {
        return true;
    }
    steps
        .get(&(instance_id.clone(), event_id.clone()))
        .is_some_and(|identity| {
            identity.step_id.as_ref() == Some(selected)
                || selected
                    .parse::<usize>()
                    .is_ok_and(|ordinal| ordinal == identity.index.saturating_add(1))
        })
}

fn render_record(
    original: &[u8],
    record: &RunEventRecord,
    format: LogFormatArg,
) -> anyhow::Result<Vec<u8>> {
    if format == LogFormatArg::Jsonl {
        let mut output = original.to_vec();
        output.push(b'\n');
        return Ok(output);
    }
    let RunEvent::Log {
        instance_id,
        step_event_id,
        text,
        partial,
        ..
    } = &record.event
    else {
        return Ok(Vec::new());
    };
    let scope = step_event_id.as_deref().map_or_else(
        || instance_id.clone(),
        |step| format!("{instance_id} > {step}"),
    );
    let text = crate::render::terminal::inline_escape(text);
    let suffix = if *partial { "" } else { "\n" };
    Ok(format!("[{scope}] {text}{suffix}").into_bytes())
}

fn write_rendered(rendered: &[u8]) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output.write_all(rendered).map_err(|error| {
        anyhow::anyhow!(
            "could not render stored logs: {error}\n  fix: ensure stdout is writable, then retry"
        )
    })
}
