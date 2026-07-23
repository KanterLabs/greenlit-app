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

use std::ffi::OsString;

use clap::Parser;

fn main() -> std::process::ExitCode {
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(exit_code) => return exit_code,
    };
    let result = match cli.command {
        cli::Command::Plan(args) => plan_cmd::run(args),
        cli::Command::Stats => stats_cmd::run(),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Error reporting itself must not turn a closed stderr pipe into
            // a panic. There is no second output channel to report this
            // write failure on, so preserve the command's failure status.
            let stderr = std::io::stderr();
            let mut handle = stderr.lock();
            let _write_result = render::terminal::write_sanitized(&mut handle, &format!("{e}\n"));
            std::process::ExitCode::FAILURE
        }
    }
}

fn parse_cli() -> Result<cli::Cli, std::process::ExitCode> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    match cli::Cli::try_parse_from(arguments.clone()) {
        Ok(cli) => Ok(cli),
        Err(error) => {
            let use_stderr = error.use_stderr();
            let exit_code = u8::try_from(error.exit_code()).unwrap_or(1);

            // clap includes invalid argument text in its diagnostic. Parse a
            // display-only copy whose untrusted OS arguments have already
            // been made terminal-safe, while retaining the original values
            // for every successful parse (notably workflow paths containing
            // control bytes).
            let safe_arguments = arguments
                .iter()
                .map(|argument| {
                    OsString::from(render::terminal::inline_escape(&argument.to_string_lossy()))
                })
                .collect::<Vec<_>>();
            let rendered = cli::Cli::try_parse_from(safe_arguments).map_or_else(
                |safe_error| safe_error.to_string(),
                |_| "could not parse command-line arguments\n".to_string(),
            );
            let write_result = if use_stderr {
                let stderr = std::io::stderr();
                render::terminal::write_sanitized(&mut stderr.lock(), &rendered)
            } else {
                let stdout = std::io::stdout();
                render::terminal::write_sanitized(&mut stdout.lock(), &rendered)
            };
            if write_result.is_err() {
                Err(std::process::ExitCode::FAILURE)
            } else {
                Err(std::process::ExitCode::from(exit_code))
            }
        }
    }
}
