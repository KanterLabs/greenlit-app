//! Read-only local-state diagnosis.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

use crate::cli::DoctorArgs;

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
