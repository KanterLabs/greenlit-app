#![forbid(unsafe_code)]

//! `litci` -- the Greenlit CLI binary (`greenlit-v0-spec.md`).
//!
//! `plan`/`stats` landed in Phase 1, `run`/`setup` in Phase 2, `auth` in
//! Phase 3 (`PHASE-3-actions.md` Auth), and `clean` in Phase 4.

mod auth;
mod auth_cmd;
mod clean_cmd;
mod cli;
mod daemon;
mod doctor_cmd;
mod dotenv_format;
mod errors;
mod gh_names;
mod github_confirmation;
mod inspect_cmd;
mod plan_cmd;
mod render;
mod run_cmd;
mod run_evidence;
mod runtime_token;
mod secrets;
mod setup_cmd;
mod stats_cmd;
mod vars;
mod workflow_discovery;
mod workflow_picker;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(exit_code) => return exit_code,
    };
    let result = match cli.command {
        cli::Command::Plan(args) => plan_cmd::run(args).map(|()| ExitCode::SUCCESS),
        cli::Command::Run(args) => run_cmd::run(args),
        cli::Command::Setup(args) => setup_cmd::run(args),
        cli::Command::Auth(args) => auth_cmd::run(args),
        cli::Command::Stats => stats_cmd::run().map(|()| ExitCode::SUCCESS),
        cli::Command::Inspect(args) => inspect_cmd::run(args).map(|()| ExitCode::SUCCESS),
        cli::Command::Export(args) => github_confirmation::export(args).map(|()| ExitCode::SUCCESS),
        cli::Command::Confirm(args) => github_confirmation::confirm(args),
        cli::Command::Doctor(args) => doctor_cmd::run(args),
        cli::Command::Clean(args) => clean_cmd::run(args),
        cli::Command::Daemon(args) => daemon::command(args),
    };
    match result {
        Ok(code) => code,
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
