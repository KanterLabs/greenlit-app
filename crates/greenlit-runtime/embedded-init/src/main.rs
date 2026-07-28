//! The `greenlit-init` binary: a thin shell over the crate library.
//!
//! It parses the container-entrypoint arguments, establishes workspace
//! isolation, and `exec(2)`s the job command. See the `greenlit_init` crate
//! docs for the runtime and embedding contracts.
#![deny(unsafe_code)]

use std::process::ExitCode;

use greenlit_init::{Args, run};

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("greenlit-init: {err}");
            // Usage error: distinct from a runtime failure so the caller can
            // tell a malformed invocation from a mount/exec problem.
            return ExitCode::from(2);
        }
    };

    match run(&args) {
        // `run` returns only on failure; the success path `exec`s away. The
        // `Ok` payload is uninhabited, so this arm discharges it without a
        // panic.
        Ok(never) => match never {},
        Err(err) => {
            eprintln!("greenlit-init: {err}");
            ExitCode::from(1)
        }
    }
}
