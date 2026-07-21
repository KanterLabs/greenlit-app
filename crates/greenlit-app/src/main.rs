#![forbid(unsafe_code)]

//! `litci` -- the Greenlit CLI binary (`greenlit-v0-spec.md`).
//!
//! Phase 1 implements exactly `plan` and `stats`
//! (`PHASE-1-engine-core.md` greenlit-app section); `run`/`auth`/`setup`/
//! `clean` are later-phase commands and are not wired up here.

mod cli;
mod errors;
mod plan_cmd;
mod render;
mod stats_cmd;
mod vars;
mod workflow_discovery;

use clap::Parser;

fn main() -> std::process::ExitCode {
    let cli = cli::Cli::parse();
    let result = match cli.command {
        cli::Command::Plan(args) => plan_cmd::run(args),
        cli::Command::Stats => stats_cmd::run(),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}
