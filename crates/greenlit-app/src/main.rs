#![forbid(unsafe_code)]

//! `litci` -- the Greenlit CLI binary (`greenlit-v0-spec.md`).
//!
//! `plan`/`stats` landed in Phase 1, `run`/`setup` in Phase 2, `auth` in
//! Phase 3 (`PHASE-3-actions.md` Auth), and `clean` in Phase 4.

mod auth;
mod auth_cmd;
mod clean_cmd;
mod cli;
mod doctor_cmd;
mod dotenv_format;
mod errors;
mod gh_names;
mod inspect_cmd;
mod logs_cmd;
mod plan_cmd;
mod render;
mod retained_secret_scan;
mod run_cmd;
mod run_events;
mod run_evidence;
mod run_quarantine;
mod run_selection;
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

const PHASE_27_STABILIZATION_DIAGNOSTIC: &str = "`litci export` and `litci confirm` are disabled until Phase 27 certifies verified consumers; retry after Phase 27 is complete";
const PHASE_25_STABILIZATION_DIAGNOSTIC: &str = "`litci daemon` is disabled until Phase 25 certifies preparation, recovery, and credential containment; retry after Phase 25 is complete";

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
        cli::Command::Logs(args) => logs_cmd::run(args).map(|()| ExitCode::SUCCESS),
        cli::Command::Export(_) | cli::Command::Confirm(_) => {
            Err(anyhow::anyhow!(PHASE_27_STABILIZATION_DIAGNOSTIC))
        }
        cli::Command::Doctor(args) => doctor_cmd::run(args),
        cli::Command::Clean(args) => clean_cmd::run(args),
        cli::Command::Daemon(_) => Err(anyhow::anyhow!(PHASE_25_STABILIZATION_DIAGNOSTIC)),
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
            // been made terminal-safe. Secret-option values are replaced
            // before this second parse so malformed `KEY=VALUE` input cannot
            // be reflected through clap. The original values remain the only
            // values used after a successful parse.
            let (safe_arguments, contained_exact_secret) = display_safe_arguments(&arguments);
            let rendered = if contained_exact_secret {
                "could not parse command-line arguments containing a secret value\n  fix: pass each -s/--secret as KEY=VALUE, then retry\n"
                    .to_string()
            } else {
                cli::Cli::try_parse_from(safe_arguments).map_or_else(
                    |safe_error| safe_error.to_string(),
                    |_| "could not parse command-line arguments\n".to_string(),
                )
            };
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

/// Produces arguments safe to hand back to clap for diagnostic rendering.
///
/// Every supported spelling of the secret option is recognized:
/// `--secret VALUE`, `--secret=VALUE`, `-s VALUE`, and `-sVALUE`. Values are
/// never decoded or retained in the display copy. A long option name one
/// edit away from `secret` is treated as an intended secret option too, but
/// its typoed name is retained so clap can still render the useful unknown
/// argument and nearest-option diagnostic.
fn display_safe_arguments(arguments: &[OsString]) -> (Vec<OsString>, bool) {
    const REDACTED: &str = "GREENLIT_REDACTED=redacted";

    let mut safe = Vec::with_capacity(arguments.len());
    let mut redact_next = false;
    let mut contained_exact_secret = false;

    for (index, argument) in arguments.iter().enumerate() {
        if index == 0 {
            safe.push(OsString::from(render::terminal::inline_escape(
                &argument.to_string_lossy(),
            )));
            continue;
        }
        if redact_next {
            safe.push(OsString::from(REDACTED));
            redact_next = false;
            continue;
        }

        let rendered = argument.to_string_lossy();
        if rendered == "--secret" || rendered == "-s" {
            contained_exact_secret = true;
            redact_next = true;
            safe.push(OsString::from(rendered.as_ref()));
        } else if rendered.starts_with("--secret=") {
            contained_exact_secret = true;
            safe.push(OsString::from(format!("--secret={REDACTED}")));
        } else if rendered.starts_with("-s") && rendered.len() > 2 {
            contained_exact_secret = true;
            safe.push(OsString::from(format!("-s{REDACTED}")));
        } else if let Some((name, _)) = rendered
            .strip_prefix("--")
            .and_then(|long_option| long_option.split_once('='))
            && resembles_secret_option(name)
        {
            safe.push(OsString::from(format!("--{name}={REDACTED}")));
        } else if let Some(name) = rendered.strip_prefix("--")
            && resembles_secret_option(name)
        {
            redact_next = true;
            safe.push(OsString::from(rendered.as_ref()));
        } else {
            safe.push(OsString::from(render::terminal::inline_escape(&rendered)));
        }
    }

    (safe, contained_exact_secret)
}

/// Whether `name` is no more than one insertion, deletion, substitution, or
/// adjacent transposition away from the ASCII long-option name `secret`.
fn resembles_secret_option(name: &str) -> bool {
    const EXPECTED: &[u8] = b"secret";

    let candidate = name.as_bytes();
    if !candidate.is_ascii() {
        return false;
    }
    match candidate.len().cmp(&EXPECTED.len()) {
        std::cmp::Ordering::Less if candidate.len() + 1 == EXPECTED.len() => {
            one_insertion_apart(candidate, EXPECTED)
        }
        std::cmp::Ordering::Greater if candidate.len() == EXPECTED.len() + 1 => {
            one_insertion_apart(EXPECTED, candidate)
        }
        std::cmp::Ordering::Equal => {
            let differences = candidate
                .iter()
                .zip(EXPECTED)
                .enumerate()
                .filter_map(|(index, (actual, expected))| (actual != expected).then_some(index))
                .collect::<Vec<_>>();
            differences.len() <= 1
                || (differences.len() == 2
                    && differences[1] == differences[0] + 1
                    && candidate[differences[0]] == EXPECTED[differences[1]]
                    && candidate[differences[1]] == EXPECTED[differences[0]])
        }
        _ => false,
    }
}

/// Whether inserting one byte into `shorter` can produce `longer`.
fn one_insertion_apart(shorter: &[u8], longer: &[u8]) -> bool {
    if longer.len() != shorter.len() + 1 {
        return false;
    }

    let mut shorter_index = 0;
    let mut skipped = false;
    for byte in longer {
        if shorter_index < shorter.len() && *byte == shorter[shorter_index] {
            shorter_index += 1;
        } else if skipped {
            return false;
        } else {
            skipped = true;
        }
    }
    shorter_index == shorter.len()
}
