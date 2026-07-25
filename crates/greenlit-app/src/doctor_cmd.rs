//! Read-only local-state diagnosis and conservative interrupted-run recovery.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use greenlit_engine::{
    ExecutionConclusion, ExecutionResultV1, ResultEvidence, RunLockV1, SupportReport, TraceEventV1,
};
use serde::Serialize;

use crate::cli::DoctorArgs;

const PRELOCK_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize)]
struct DoctorDocument {
    consistent: bool,
    active_leases: u64,
    reclaimable_objects: usize,
    reclaimable_bytes: u64,
    partial_downloads: usize,
    partial_bytes: u64,
    interrupted_runs: Vec<String>,
    issues: Vec<String>,
}

pub(crate) fn run(args: DoctorArgs) -> anyhow::Result<ExitCode> {
    let home = home()?;
    let store = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(&home),
    )
    .map_err(store_error)?;
    let report = store.doctor().map_err(store_error)?;
    let interrupted_runs = interrupted_runs(&home.join(".litci/runs"))?;
    let document = DoctorDocument {
        consistent: report.is_consistent(),
        active_leases: report.active_leases,
        reclaimable_objects: report.reclaimable_objects,
        reclaimable_bytes: report.reclaimable_bytes,
        partial_downloads: report.partial_downloads,
        partial_bytes: report.partial_bytes,
        interrupted_runs,
        issues: report.issues,
    };
    if args.json {
        serde_json::to_writer_pretty(std::io::stdout().lock(), &document).map_err(|error| {
            anyhow::anyhow!(
                "could not render doctor report: {error}\n  fix: ensure stdout is writable, then retry"
            )
        })?;
        println!();
    } else {
        println!(
            "Storage metadata: {}",
            if document.consistent {
                "consistent"
            } else {
                "INCONSISTENT — destructive GC is blocked"
            }
        );
        println!("Active leases: {}", document.active_leases);
        println!(
            "Reclaimable immutable content: {} object(s), {} bytes",
            document.reclaimable_objects, document.reclaimable_bytes
        );
        println!(
            "Interrupted partial downloads: {} file(s), {} bytes",
            document.partial_downloads, document.partial_bytes
        );
        println!("Interrupted runs: {}", document.interrupted_runs.len());
        for run in &document.interrupted_runs {
            println!("  {run}");
        }
        for issue in &document.issues {
            println!("  issue: {issue}");
        }
        println!("No data was deleted.");
    }
    Ok(if document.consistent {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub(crate) fn reconcile_interrupted_runs(litci_root: &Path) -> anyhow::Result<()> {
    let home = litci_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Greenlit state root has no home directory"))?;
    let store = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(home),
    )
    .map_err(store_error)?;
    let runs = litci_root.join("runs");
    let Ok(entries) = fs::read_dir(&runs) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            anyhow::anyhow!(
                "could not inspect run recovery state: {error}\n  fix: run `litci doctor`"
            )
        })?;
        let path = entry.path();
        let Some(run_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !path.is_dir()
            || path.join("result.json").exists()
            || store.lease_is_active(&run_id).map_err(store_error)?
            || !old_enough(&path)?
        {
            continue;
        }
        let support = read_support(&path)?;
        let result = ExecutionResultV1::classify(&ResultEvidence {
            conclusion: ExecutionConclusion::Aborted,
            support,
            clean: false,
            hermetic: false,
            github_confirmed: false,
        });
        write_json_atomic(&path.join("result.json"), &result)?;
        append_recovery_trace(&path)?;
        store
            .record_run_state(&run_id, None, "aborted")
            .map_err(store_error)?;
    }
    Ok(())
}

fn interrupted_runs(root: &Path) -> anyhow::Result<Vec<String>> {
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(Vec::new());
    };
    let mut runs = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir() && !entry.path().join("result.json").exists())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    runs.sort();
    Ok(runs)
}

fn old_enough(path: &Path) -> anyhow::Result<bool> {
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| {
            anyhow::anyhow!(
                "could not inspect interrupted run {}: {error}\n  fix: run `litci doctor`",
                path.display()
            )
        })?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        >= PRELOCK_GRACE)
}

fn read_support(path: &Path) -> anyhow::Result<SupportReport> {
    let lock = path.join("run-lock.json");
    if !lock.is_file() {
        return Ok(SupportReport::default());
    }
    let bytes = fs::read(&lock).map_err(|error| evidence_error(&lock, error))?;
    serde_json::from_slice::<RunLockV1>(&bytes)
        .map(|lock| lock.compatibility)
        .map_err(|error| evidence_error(&lock, error))
}

fn append_recovery_trace(path: &Path) -> anyhow::Result<()> {
    let trace_path = path.join("trace.ndjson");
    let sequence = fs::read_to_string(&trace_path)
        .map(|contents| contents.lines().count() as u64 + 1)
        .unwrap_or(1);
    let event = TraceEventV1::new(sequence, "run_recovered", Default::default());
    let mut trace = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&trace_path)
        .map_err(|error| evidence_error(&trace_path, error))?;
    serde_json::to_writer(&mut trace, &event)
        .map_err(|error| evidence_error(&trace_path, error))?;
    trace
        .write_all(b"\n")
        .and_then(|()| trace.sync_all())
        .map_err(|error| evidence_error(&trace_path, error))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| evidence_error(&temp, error))?;
    serde_json::to_writer(&mut file, value).map_err(|error| evidence_error(&temp, error))?;
    file.write_all(b"\n")
        .and_then(|()| file.sync_all())
        .map_err(|error| evidence_error(&temp, error))?;
    fs::rename(&temp, path).map_err(|error| evidence_error(path, error))
}

fn home() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set\n  fix: set HOME, then retry"))?;
    if !home.is_absolute() {
        anyhow::bail!("HOME is not absolute\n  fix: set HOME to an absolute directory");
    }
    Ok(home)
}

fn store_error(error: greenlit_store::cas::CasError) -> anyhow::Error {
    anyhow::anyhow!(
        "could not inspect Greenlit storage: {error}\n  fix: preserve ~/.litci and repair the issue reported by `litci doctor`"
    )
}

fn evidence_error(path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "could not recover run evidence at {}: {error}\n  fix: preserve the directory and run `litci doctor`",
        path.display()
    )
}
