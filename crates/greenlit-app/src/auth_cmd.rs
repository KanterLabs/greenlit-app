//! `litci auth`: device-flow login by default, `--pat` to paste a
//! fine-grained personal access token, `--gh` to reuse `gh auth token`.
//!
//! `PHASE-3-actions.md` Auth. All the actual wire/storage work lives in
//! `crate::auth`; this module is only the thin command entry point,
//! mirroring `crate::setup_cmd`'s shape.

use std::io;
use std::process::ExitCode;

use crate::auth;
use crate::cli::AuthArgs;

/// Runs `litci auth[, --pat, --gh]`, returning the process exit code.
///
/// Failures are returned as `anyhow::Error` (rather than printed here
/// directly) so `main.rs`'s top-level handler sanitizes them the same way
/// every other command's errors are — GitHub's own responses are a lower-risk
/// source than repository content, but the terminal-escaping invariant
/// (`AGENTS.md`) is unconditional, not content-source-dependent.
pub(crate) fn run(args: AuthArgs) -> anyhow::Result<ExitCode> {
    let mut out = io::stdout();
    let result = if args.pat {
        auth::run_pat_flow(&mut out)
    } else if args.gh {
        auth::run_gh_flow(&mut out)
    } else {
        auth::run_device_flow(&mut out)
    };
    result
        .map(|()| ExitCode::SUCCESS)
        .map_err(|message| anyhow::anyhow!(message))
}
