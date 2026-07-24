//! Orchestration: establish the isolated workspace, then `exec(2)` the job
//! command in it.

use std::convert::Infallible;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::{Args, StrategyPref};
use crate::error::InitError;
use crate::status::StatusWriter;
use crate::strategy::{self, FALLBACK_MARKER, Isolation};
use crate::{copy_in, mount};

/// Establish workspace isolation per `args`, then replace this process with the
/// job command running in the workspace.
///
/// On success this never returns — `exec(2)` replaces the process image, so the
/// job command becomes the entrypoint's successor. It returns `Err` only when
/// isolation setup or the `exec` itself fails; the `Ok` variant is
/// uninhabited ([`Infallible`]) to make that non-return explicit in the type.
///
/// # Errors
///
/// Returns an [`InitError`] describing the failed stage: directory preparation,
/// overlay option encoding, a required-but-failed or unexpectedly-failed mount,
/// the copy-in fallback, or the final `exec`.
pub fn run(args: &Args) -> Result<Infallible, InitError> {
    let status = StatusWriter::new();
    // The host cannot read this process's stderr, so on any failure the
    // status file carries the real error for the host to surface.
    if let Err(error) = prepare_isolation(args, &status) {
        status.failed(&error.to_string());
        return Err(error);
    }
    let error = match exec_command(args) {
        Ok(never) => match never {},
        Err(error) => error,
    };
    status.failed(&error.to_string());
    Err(error)
}

/// Set up the workspace via overlay or copy-in, honoring `args.strategy`,
/// reporting phases and copy progress through `status`. Ends with the
/// `done` snapshot; the ready marker (touched by the idle command after the
/// exec) remains the host's terminal readiness signal.
fn prepare_isolation(args: &Args, status: &StatusWriter) -> Result<(), InitError> {
    status.start();
    create_dir(&args.workspace)?;

    match args.strategy {
        StrategyPref::CopyIn => {
            populate_with_status(args, status, None)?;
            status.done("copy-in", None);
            Ok(())
        }
        StrategyPref::Auto | StrategyPref::Overlay => {
            let require_overlay = matches!(args.strategy, StrategyPref::Overlay);
            let (upper, work) = prepare_overlay_dirs(&args.upper)?;
            let options = strategy::overlay_options(&args.lower, &upper, &work)?;

            status.overlay();
            let mount_errno = match mount::mount_overlay(&args.workspace, &options) {
                Ok(()) => None,
                // A failed mount always carries an OS errno; `-1` is an
                // impossible sentinel that classifies as a fatal, non-maskable
                // error if the platform ever omitted one.
                Err(e) => Some(e.raw_os_error().unwrap_or(-1)),
            };

            match strategy::decide_after_overlay_attempt(require_overlay, mount_errno)? {
                Isolation::Overlay => {
                    status.done("overlay", None);
                    Ok(())
                }
                Isolation::CopyIn { fell_back } => {
                    let reason = fell_back.then(|| {
                        mount_errno.map_or_else(|| "unknown".to_string(), strategy::errno_name)
                    });
                    if let Some(reason) = &reason {
                        eprintln!(
                            "{FALLBACK_MARKER} strategy=copy-in reason={reason}: unprivileged \
                             overlayfs unavailable, copying the checkout into the workspace"
                        );
                    }
                    populate_with_status(args, status, reason.as_deref())?;
                    status.done("copy-in", reason.as_deref());
                    Ok(())
                }
            }
        }
    }
}

/// Run the checkout copy, streaming file/byte counts into the status file.
fn populate_with_status(
    args: &Args,
    status: &StatusWriter,
    fell_back: Option<&str>,
) -> Result<(), InitError> {
    status.copy(0, 0, fell_back);
    copy_in::populate(&args.lower, &args.workspace, &mut |files, bytes| {
        status.copy(files, bytes, fell_back);
    })
}

/// Create the overlay `upper/` and `work/` directories under `base`.
fn prepare_overlay_dirs(base: &Path) -> Result<(PathBuf, PathBuf), InitError> {
    let upper = base.join("upper");
    let work = base.join("work");
    create_dir(&upper)?;
    create_dir(&work)?;
    Ok((upper, work))
}

/// Create a directory (and parents), mapping failure to [`InitError::PrepareDir`].
fn create_dir(path: &Path) -> Result<(), InitError> {
    fs::create_dir_all(path).map_err(|source| InitError::PrepareDir {
        path: path.to_path_buf(),
        source,
    })
}

/// Replace this process with the job command, running in the workspace.
///
/// [`CommandExt::exec`] returns only on failure, so the `Ok(Infallible)` arm is
/// unreachable and the function always yields `Err` when it returns at all.
fn exec_command(args: &Args) -> Result<Infallible, InitError> {
    let program = &args.command[0];
    let error = Command::new(program)
        .args(&args.command[1..])
        .current_dir(&args.workspace)
        .exec();
    Err(InitError::Exec {
        program: program.clone(),
        source: error,
    })
}
