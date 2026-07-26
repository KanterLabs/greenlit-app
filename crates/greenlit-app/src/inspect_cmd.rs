//! Read-only rendering of persisted run evidence.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::cli::InspectArgs;

pub(crate) fn run(args: InspectArgs) -> anyhow::Result<()> {
    let runs = runs_root()?;
    let run_id = match args.run_id {
        Some(run_id) => validate_run_id(&run_id)?,
        None => latest_run_id(&runs)?,
    };
    let directory = runs.join(&run_id);
    if !directory.is_dir() {
        anyhow::bail!(
            "run evidence '{run_id}' does not exist\n  fix: run `litci inspect` without an ID to inspect the latest run"
        );
    }
    let lock = read_json(&directory.join("run-lock.json"))?;
    let result_path = directory.join("result.json");
    let result = if result_path.is_file() {
        Some(read_json(&result_path)?)
    } else {
        None
    };
    let document = serde_json::json!({
        "run_id": run_id,
        "lock": lock,
        "result": result,
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
    Ok(home.join(".litci").join("runs"))
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

fn latest_run_id(runs: &Path) -> anyhow::Result<String> {
    let entries = fs::read_dir(runs).map_err(|error| {
        anyhow::anyhow!(
            "could not list run evidence at {}: {error}\n  fix: run a workflow first, or make HOME readable",
            runs.display()
        )
    })?;
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| validate_run_id(name).is_ok())
        .max()
        .ok_or_else(|| {
            anyhow::anyhow!("no local run evidence exists\n  fix: run `litci run` once, then retry")
        })
}

fn read_json(path: &Path) -> anyhow::Result<serde_json::Value> {
    let bytes = fs::read(path).map_err(|error| {
        anyhow::anyhow!(
            "could not read run evidence {}: {error}\n  fix: choose a completed run or use `litci doctor` to diagnose local state",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "run evidence {} is not valid JSON: {error}\n  fix: preserve the directory and use `litci doctor` to diagnose local state",
            path.display()
        )
    })
}
