//! Read-only rendering of persisted run evidence.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;

use greenlit_engine::{ExecutionResultV1, TraceEventV1};
use greenlit_store::cas::{CasStore, RunCatalogState};
use serde::de::DeserializeOwned;

use crate::cli::InspectArgs;
use crate::run_events::{RunEvent, RunEventRecord};

mod private_reader;

pub(crate) use private_reader::{RetainedRun, RunsDirectory, open_runs_directory};

pub(crate) struct TerminalAuthority {
    pub(crate) result: ExecutionResultV1,
}

pub(crate) fn run(args: InspectArgs) -> anyhow::Result<()> {
    let requested = args.run_id.as_deref().map(validate_run_id).transpose()?;
    let runs = open_runs_directory()?;
    let run_id = match requested {
        Some(run_id) => run_id,
        None => latest_run_id(&runs)?,
    };
    let retained = runs.open_run(&run_id)?;
    let store = open_catalog_store()?;
    let authority = terminal_authority(&retained, &store)?;
    let lock: serde_json::Value = retained.read_json("run-lock.json", "run lock")?;
    let document = serde_json::json!({
        "run_id": run_id,
        "lock": lock,
        "result": authority.result,
        "terminal_authority": {
            "authoritative": true,
            "catalog_state": "completed",
            "result": "valid",
            "run_finished": "matching",
            "run_completed_trace": "matching",
        },
    });
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &document).map_err(|error| {
        anyhow::anyhow!(
            "could not render run evidence: {error}\n  fix: ensure stdout is writable, then retry"
        )
    })?;
    output.write_all(b"\n").map_err(|error| {
        anyhow::anyhow!(
            "could not finish rendering run evidence: {error}\n  fix: ensure stdout is writable, then retry"
        )
    })
}

pub(crate) fn runs_root() -> anyhow::Result<PathBuf> {
    Ok(home()?.join(".litci").join("runs"))
}

pub(crate) fn open_catalog_store() -> anyhow::Result<CasStore> {
    let home = home()?;
    CasStore::open(CasStore::default_path_under(&home)).map_err(|error| {
        anyhow::anyhow!(
            "could not read the durable run catalog: {error}\n  fix: preserve ~/.litci and use `litci doctor` to diagnose local state"
        )
    })
}

pub(crate) fn validate_run_id(run_id: &str) -> anyhow::Result<String> {
    if run_id.is_empty()
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        anyhow::bail!(
            "invalid run identity '{run_id}'\n  fix: copy the exact run identity printed by `litci run`"
        );
    }
    Ok(run_id.to_string())
}

pub(crate) fn terminal_authority(
    run: &RetainedRun,
    store: &CasStore,
) -> anyhow::Result<TerminalAuthority> {
    let state = store.run_state(run.run_id()).map_err(|error| {
        anyhow::anyhow!(
            "could not read durable state for run '{}': {error}\n  fix: preserve ~/.litci and use `litci doctor`",
            run.run_id()
        )
    })?;
    match state {
        Some(RunCatalogState::Completed) => {}
        Some(RunCatalogState::Aborted) => {
            anyhow::bail!(
                "run '{}' is aborted; retained completion-looking files are not authoritative\n  fix: choose another run or use `litci doctor` to inspect recovery state",
                run.run_id()
            );
        }
        Some(state) => {
            anyhow::bail!(
                "run '{}' is not durably completed (catalog state: {state}); retained completion-looking files are not authoritative\n  fix: wait for the active run, or use `litci doctor` after an interruption",
                run.run_id()
            );
        }
        None => {
            anyhow::bail!(
                "run '{}' has no durable catalog state; retained completion-looking files are not authoritative\n  fix: preserve the run directory and use `litci doctor`",
                run.run_id()
            );
        }
    }

    let result: ExecutionResultV1 = run.read_json("result.json", "run result")?;
    if result.schema_version != 1 {
        anyhow::bail!(
            "run '{}' has unsupported result schema version {}\n  fix: inspect it with the Greenlit version that created it",
            run.run_id(),
            result.schema_version
        );
    }
    validate_finished_event(run, &result)?;
    validate_completed_trace(run, &result)?;
    Ok(TerminalAuthority { result })
}

fn validate_finished_event(run: &RetainedRun, result: &ExecutionResultV1) -> anyhow::Result<()> {
    let record: RunEventRecord = read_last_record(run, "events.ndjson", "run journal")?;
    if record.schema_version != 1 || record.run_id != run.run_id() {
        anyhow::bail!(
            "run '{}' has a terminal journal record with inconsistent schema or identity\n  fix: preserve the run directory and use `litci doctor`",
            run.run_id()
        );
    }
    let RunEvent::RunFinished {
        conclusion,
        compatibility,
        assurance,
        evidence,
    } = record.event
    else {
        anyhow::bail!(
            "run '{}' is catalog-completed but RunFinished is not the final durable journal record\n  fix: preserve the run directory and use `litci doctor`",
            run.run_id()
        );
    };
    let expected = (
        format!("{:?}", result.conclusion),
        format!("{:?}", result.compatibility),
        format!("{:?}", result.assurance),
        run.run_id().to_string(),
    );
    if (conclusion, compatibility, assurance, evidence) != expected {
        anyhow::bail!(
            "run '{}' has a RunFinished event that does not match result.json\n  fix: preserve the run directory and use `litci doctor`",
            run.run_id()
        );
    }
    Ok(())
}

fn validate_completed_trace(run: &RetainedRun, result: &ExecutionResultV1) -> anyhow::Result<()> {
    let record: TraceEventV1 = read_last_record(run, "trace.ndjson", "run trace")?;
    if record.schema_version != 1 || record.event != "run_completed" {
        anyhow::bail!(
            "run '{}' is catalog-completed but run_completed is not the final durable trace record\n  fix: preserve the run directory and use `litci doctor`",
            run.run_id()
        );
    }
    let expected = BTreeMap::from([
        ("conclusion".to_string(), format!("{:?}", result.conclusion)),
        (
            "compatibility".to_string(),
            format!("{:?}", result.compatibility),
        ),
        ("assurance".to_string(), format!("{:?}", result.assurance)),
    ]);
    if record.attributes != expected {
        anyhow::bail!(
            "run '{}' has a run_completed trace that does not match result.json\n  fix: preserve the run directory and use `litci doctor`",
            run.run_id()
        );
    }
    Ok(())
}

fn read_last_record<T: DeserializeOwned>(
    run: &RetainedRun,
    name: &'static str,
    description: &'static str,
) -> anyhow::Result<T> {
    let path = run.artifact_path(name);
    let mut reader = BufReader::new(run.open_artifact(name)?);
    let mut line = Vec::new();
    let mut last = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).map_err(|error| {
            anyhow::anyhow!(
                "could not read {description} {}: {error}\n  fix: preserve the run directory and use `litci doctor`",
                path.display()
            )
        })?;
        if read == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            anyhow::bail!(
                "{description} {} ends with an incomplete record\n  fix: preserve the run directory and use `litci doctor`",
                path.display()
            );
        }
        if line.iter().any(|byte| !byte.is_ascii_whitespace()) {
            last.clone_from(&line);
        }
    }
    if last.is_empty() {
        anyhow::bail!(
            "{description} {} has no durable records\n  fix: preserve the run directory and use `litci doctor`",
            path.display()
        );
    }
    serde_json::from_slice(&last).map_err(|error| {
        anyhow::anyhow!(
            "{description} {} has an invalid final record: {error}\n  fix: preserve the run directory and use `litci doctor`",
            path.display()
        )
    })
}

fn latest_run_id(runs: &RunsDirectory) -> anyhow::Result<String> {
    runs.run_ids()?.into_iter().max().ok_or_else(|| {
        anyhow::anyhow!("no local run evidence exists\n  fix: run `litci run` once, then retry")
    })
}

fn home() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        anyhow::anyhow!(
            "could not find run evidence because HOME is not set\n  fix: set HOME to an absolute directory, then retry"
        )
    })?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        anyhow::bail!(
            "could not find run evidence because HOME is not absolute\n  fix: set HOME to an absolute directory, then retry"
        );
    }
    Ok(home)
}
