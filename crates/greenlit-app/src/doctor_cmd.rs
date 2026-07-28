//! Read-only local-state diagnosis.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;

use greenlit_store::cas::{RunCatalogState, RunPublicationLockState};
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
    active_runs: Vec<String>,
    aborted_runs: Vec<String>,
    terminal_authority_issues: Vec<String>,
    issues: Vec<String>,
}

pub(crate) fn run(args: DoctorArgs) -> anyhow::Result<ExitCode> {
    let home = home()?;
    let store = greenlit_store::cas::CasStore::open(
        greenlit_store::cas::CasStore::default_path_under(&home),
    )
    .map_err(store_error)?;
    let report = store.doctor().map_err(store_error)?;
    let runs = match crate::inspect_cmd::open_runs_directory() {
        Ok(runs) => Some(runs),
        Err(_error) if !home.join(".litci/runs").exists() => None,
        Err(error) => return Err(error),
    };
    let run_report = inspect_runs(&store, runs.as_ref())?;
    let mut issues = report.issues;
    issues.extend(run_report.issues.iter().cloned());
    let consistent = issues.is_empty();
    let document = DoctorDocument {
        consistent,
        active_leases: report.active_leases,
        reclaimable_objects: report.reclaimable_objects,
        reclaimable_bytes: report.reclaimable_bytes,
        partial_downloads: report.partial_downloads,
        partial_bytes: report.partial_bytes,
        interrupted_runs: run_report.interrupted,
        active_runs: run_report.active,
        aborted_runs: run_report.aborted,
        terminal_authority_issues: run_report.authority_issues,
        issues,
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
        println!("Active runs: {}", document.active_runs.len());
        for run in &document.active_runs {
            println!("  {run}");
        }
        println!("Aborted runs: {}", document.aborted_runs.len());
        for run in &document.aborted_runs {
            println!("  {run}");
        }
        println!(
            "Composite terminal-authority issues: {}",
            document.terminal_authority_issues.len()
        );
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

struct RunDoctorReport {
    interrupted: Vec<String>,
    active: Vec<String>,
    aborted: Vec<String>,
    authority_issues: Vec<String>,
    issues: Vec<String>,
}

fn inspect_runs(
    store: &greenlit_store::cas::CasStore,
    runs: Option<&crate::inspect_cmd::RunsDirectory>,
) -> anyhow::Result<RunDoctorReport> {
    let entries = store.run_catalog_entries().map_err(store_error)?;
    let mut catalog_ids = BTreeSet::new();
    let mut interrupted = Vec::new();
    let mut active = Vec::new();
    let mut aborted = Vec::new();
    let mut authority_issues = Vec::new();
    let mut issues = Vec::new();

    for entry in entries {
        catalog_ids.insert(entry.run_id.clone());
        match entry.state {
            RunCatalogState::Resolved => {
                let lock = runs.map_or(Ok(RunPublicationLockState::Missing), |runs| {
                    store.run_publication_lock_state(runs.path(), &entry.run_id)
                });
                match lock.map_err(store_error)? {
                    RunPublicationLockState::Active => active.push(entry.run_id),
                    RunPublicationLockState::Inactive => {
                        issues.push(format!(
                            "run {} is inactive but its catalog state is resolved; the next run startup must abort and quarantine it",
                            entry.run_id
                        ));
                        interrupted.push(entry.run_id);
                    }
                    RunPublicationLockState::Missing => {
                        issues.push(format!(
                            "run {} is resolved but lacks the private publication lock required for safe automatic recovery",
                            entry.run_id
                        ));
                        interrupted.push(entry.run_id);
                    }
                }
            }
            RunCatalogState::Aborted => {
                if let Some(runs) = runs
                    && runs.run_ids()?.binary_search(&entry.run_id).is_ok()
                {
                    issues.push(format!(
                        "aborted run {} still has a tree in the authoritative runs directory; startup recovery must quarantine it",
                        entry.run_id
                    ));
                }
                aborted.push(entry.run_id);
            }
            RunCatalogState::Completed => {
                let authority = runs
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "completed run {} has no retained runs directory",
                            entry.run_id
                        )
                    })
                    .and_then(|runs| runs.open_run(&entry.run_id))
                    .and_then(|run| crate::inspect_cmd::terminal_authority(&run, store));
                if let Err(error) = authority {
                    let issue = format!(
                        "completed run {} lacks composite terminal authority: {error}",
                        entry.run_id
                    );
                    authority_issues.push(issue.clone());
                    issues.push(issue);
                }
            }
        }
    }

    if let Some(runs) = runs {
        for run_id in runs.run_ids()? {
            if !catalog_ids.contains(&run_id) {
                let run = runs.open_run(&run_id)?;
                if run.has_artifact("result.json")? {
                    let issue = format!(
                        "run {run_id} has result.json but no durable catalog row; the file is non-authoritative"
                    );
                    authority_issues.push(issue.clone());
                    issues.push(issue);
                } else {
                    interrupted.push(run_id);
                }
            }
        }
    }

    interrupted.sort();
    active.sort();
    aborted.sort();
    authority_issues.sort();
    issues.sort();
    Ok(RunDoctorReport {
        interrupted,
        active,
        aborted,
        authority_issues,
        issues,
    })
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
