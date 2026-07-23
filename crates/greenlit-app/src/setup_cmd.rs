//! `litci setup`: the three-state container-engine UX.
//!
//! `greenlit-v0-spec.md` ("Zero prerequisites") and `PHASE-2-execution.md`
//! ("Three-state engine detection"): reachable → report and exit; installed but
//! the daemon is stopped → offer to start it after one confirmation; absent →
//! install Docker via its official script after one confirmation. Greenlit never
//! surfaces a raw connection error — every state carries a message plus the one
//! action that fixes it, and no privileged command ever runs unprompted.

use std::io::{self, BufRead, Write};
use std::process::{Command, ExitCode};

use greenlit_runtime::{EngineState, SystemProber, detect};

use crate::cli::SetupArgs;

/// Docker's official convenience installer (`greenlit-v0-spec.md`: "installs
/// Docker via the official script"). Run only after an explicit confirmation.
const INSTALL_SCRIPT: &str = "curl -fsSL https://get.docker.com | sudo sh";

/// Run `litci setup`, returning the process exit code.
pub(crate) fn run(args: SetupArgs) -> anyhow::Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            anyhow::anyhow!("could not start the async runtime: {error}\n  fix: retry")
        })?;
    let state = runtime.block_on(detect(&SystemProber::new()));

    match state {
        EngineState::Available { endpoint } => {
            println!("Container engine is reachable at {}.", endpoint.describe());
            println!("You're ready to run `litci run`.");
            Ok(ExitCode::SUCCESS)
        }
        EngineState::DaemonStopped(fix) => {
            println!("{}", fix.message);
            println!("Greenlit can start it for you: `{}`.", fix.action);
            if confirm("Start the container daemon now?", args.yes)? {
                run_privileged(&fix.action)?;
                report_after(&runtime)
            } else {
                println!("No changes made. Start it yourself with: {}", fix.action);
                Ok(ExitCode::FAILURE)
            }
        }
        EngineState::NotInstalled(fix) => {
            println!("{}", fix.message);
            println!(
                "Greenlit can install Docker using its official script. This downloads and runs a privileged installer:\n  {INSTALL_SCRIPT}"
            );
            if confirm("Install Docker now?", args.yes)? {
                run_privileged(INSTALL_SCRIPT)?;
                report_after(&runtime)
            } else {
                println!("No changes made. Install Docker yourself, then run `litci setup` again.");
                Ok(ExitCode::FAILURE)
            }
        }
        EngineState::UnsupportedDockerHost(fix) => {
            // Not a start/install case — there is nothing for `litci setup` to
            // run privileged; the fix is a configuration change only.
            println!("{}", fix.message);
            println!("  fix: {}", fix.action);
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Re-detect after a start/install attempt and report the resulting state.
fn report_after(runtime: &tokio::runtime::Runtime) -> anyhow::Result<ExitCode> {
    match runtime.block_on(detect(&SystemProber::new())) {
        EngineState::Available { endpoint } => {
            println!(
                "Container engine is now reachable at {}.",
                endpoint.describe()
            );
            Ok(ExitCode::SUCCESS)
        }
        EngineState::DaemonStopped(fix)
        | EngineState::NotInstalled(fix)
        | EngineState::UnsupportedDockerHost(fix) => {
            println!(
                "The container engine is still not reachable.\n  {}\n  fix: {}",
                fix.message, fix.action
            );
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Ask a yes/no question. Returns `true` immediately when `--yes` pre-confirmed.
fn confirm(question: &str, pre_confirmed: bool) -> anyhow::Result<bool> {
    if pre_confirmed {
        return Ok(true);
    }
    print!("{question} [y/N] ");
    io::stdout().flush().ok();
    let mut answer = String::new();
    let stdin = io::stdin();
    if stdin.lock().read_line(&mut answer).is_err() {
        // An unreadable prompt is treated as "no" — nothing privileged runs.
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Run a privileged shell command (daemon start or the install script),
/// inheriting the terminal so its own prompts (e.g. `sudo`) are visible.
fn run_privileged(command: &str) -> anyhow::Result<()> {
    println!("Running: {command}");
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .status()
        .map_err(|error| {
            anyhow::anyhow!(
                "could not launch `{command}`: {error}\n  fix: run the command yourself in a shell"
            )
        })?;
    if !status.success() {
        anyhow::bail!(
            "`{command}` exited unsuccessfully\n  fix: run it yourself and resolve the error it reports"
        );
    }
    Ok(())
}
