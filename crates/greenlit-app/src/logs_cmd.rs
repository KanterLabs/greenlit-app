//! Read-only replay of redacted log events from a persisted run journal.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::thread;
use std::time::Duration;

use greenlit_store::cas::RunCatalogState;

use crate::cli::{LogFormatArg, LogsArgs};
use crate::inspect_cmd::{RetainedRun, RunsDirectory};
use crate::run_events::{RunEvent, RunEventRecord};

#[derive(Debug, Clone)]
struct StepIdentity {
    index: usize,
    step_id: Option<String>,
}

pub(crate) fn run(args: LogsArgs) -> anyhow::Result<()> {
    let runs = crate::inspect_cmd::open_runs_directory()?;
    let (retained, mut journal) = select_journal(&runs, args.run_id.as_deref())?;
    let run_id = retained.run_id().to_string();
    let store = crate::inspect_cmd::open_catalog_store()?;
    let mut offset = 0;
    let mut expected_sequence = 1_u64;
    let mut saw_terminal = false;
    let mut steps = HashMap::new();
    let mut tail = VecDeque::new();
    let mut first_pass = true;
    let tail_limit = args
        .tail
        .map(|value| usize::try_from(value).unwrap_or(usize::MAX));

    loop {
        let bytes = read_appended(&mut journal, offset, &run_id)?;
        let appended = bytes.as_slice();
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
            if record.schema_version != 1
                || record.sequence != expected_sequence
                || record.run_id != run_id
            {
                anyhow::bail!(
                    "run journal for '{run_id}' has inconsistent schema, ordering, or run identity at sequence {expected_sequence}\n  fix: preserve the run directory and use `litci doctor`"
                );
            }
            expected_sequence = expected_sequence.saturating_add(1);
            index_step(&record, &mut steps);
            saw_terminal |= matches!(&record.event, RunEvent::RunFinished { .. });
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
        if !args.follow {
            break;
        }

        let recovery = store.recover_incomplete_run_publications(runs.path());
        let state = store.run_state(&run_id).map_err(|error| {
            anyhow::anyhow!(
                "could not read durable state while following run '{run_id}': {error}\n  fix: preserve ~/.litci and use `litci doctor`"
            )
        })?;
        match state {
            Some(RunCatalogState::Completed) => {
                crate::inspect_cmd::terminal_authority(&retained, &store)?;
                break;
            }
            Some(RunCatalogState::Aborted) => {
                let cleanup = recovery
                    .err()
                    .map(|error| format!("; recovery cleanup also reported: {error}"))
                    .unwrap_or_default();
                anyhow::bail!(
                    "run '{run_id}' was aborted before a composite terminal commit{cleanup}\n  fix: choose another run or use `litci doctor` to inspect recovery state"
                );
            }
            Some(RunCatalogState::Resolved) => {
                let recovery = recovery.map_err(|error| {
                    anyhow::anyhow!(
                        "could not reconcile run '{run_id}' while following its logs: {error}\n  fix: preserve ~/.litci and use `litci doctor`"
                    )
                })?;
                if recovery.unprotected.iter().any(|id| id == &run_id) {
                    anyhow::bail!(
                        "run '{run_id}' lacks the private publication lock required to distinguish an active writer from an orphan\n  fix: preserve the run directory and use `litci doctor`"
                    );
                }
                thread::sleep(Duration::from_millis(150));
            }
            None if saw_terminal => {
                anyhow::bail!(
                    "run '{run_id}' published RunFinished without any durable catalog state; the event is not authoritative\n  fix: preserve the run directory and use `litci doctor`"
                );
            }
            None => thread::sleep(Duration::from_millis(150)),
        }
    }
    Ok(())
}

fn read_appended(journal: &mut File, offset: usize, run_id: &str) -> anyhow::Result<Vec<u8>> {
    let length = journal
        .metadata()
        .map_err(|error| {
            anyhow::anyhow!(
                "could not inspect logs for run '{run_id}': {error}\n  fix: use `litci doctor` to check the run journal"
            )
        })?
        .len();
    let offset_u64 = u64::try_from(offset).map_err(|_| {
        anyhow::anyhow!(
            "run journal for '{run_id}' exceeds the supported host size\n  fix: preserve the run directory and use `litci doctor`"
        )
    })?;
    if offset_u64 > length {
        anyhow::bail!(
            "run journal for '{run_id}' shrank while it was being read\n  fix: preserve the run directory and use `litci doctor`"
        );
    }
    journal.seek(SeekFrom::Start(offset_u64)).map_err(|error| {
        anyhow::anyhow!(
            "could not seek logs for run '{run_id}': {error}\n  fix: use `litci doctor` to check the run journal"
        )
    })?;
    let mut bytes = Vec::new();
    journal.read_to_end(&mut bytes).map_err(|error| {
        anyhow::anyhow!(
            "could not read logs for run '{run_id}': {error}\n  fix: use `litci doctor` to check the run journal"
        )
    })?;
    Ok(bytes)
}

fn select_journal(
    runs: &RunsDirectory,
    requested: Option<&str>,
) -> anyhow::Result<(RetainedRun, File)> {
    if let Some(requested) = requested {
        let run_id = crate::inspect_cmd::validate_run_id(requested)?;
        let retained = runs.open_run(&run_id)?;
        if !retained.has_artifact("events.ndjson")? {
            anyhow::bail!(
                "run '{run_id}' has no structured log journal\n  fix: choose a run created by this version of Greenlit"
            );
        }
        let journal = retained.open_artifact("events.ndjson")?;
        return Ok((retained, journal));
    }
    for run_id in runs.run_ids()?.into_iter().rev() {
        let retained = runs.open_run(&run_id)?;
        if retained.has_artifact("events.ndjson")? {
            let journal = retained.open_artifact("events.ndjson")?;
            return Ok((retained, journal));
        }
    }
    Err(anyhow::anyhow!(
        "no structured run logs exist\n  fix: run `litci run` once with this version, then retry"
    ))
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
