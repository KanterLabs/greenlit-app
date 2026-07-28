//! Command-line parsing for the container entrypoint.
//!
//! The parse is deliberately tiny and dependency-free (no `clap`): this binary
//! is invoked only by Greenlit with a fixed, machine-generated argument vector,
//! so a hand-rolled parser keeps the embedded static binary small and its
//! decision logic fully unit-testable without a container.

use std::path::PathBuf;

/// The parsed container-entrypoint invocation.
///
/// Constructed by [`Args::parse`] from the process argument vector (excluding
/// `argv[0]`). See the crate-level docs for the wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// The Docker-level read-only repo bind — the overlay lower layer, and the
    /// source tree copied in on the fallback path.
    pub lower: PathBuf,
    /// A container-local writable base directory; the helper creates `upper/`
    /// and `work/` beneath it for the overlay upper and work directories.
    pub upper: PathBuf,
    /// Where the merged, writable checkout is exposed (the job's
    /// `GITHUB_WORKSPACE`).
    pub workspace: PathBuf,
    /// Which isolation mechanism to use.
    pub strategy: StrategyPref,
    /// Which per-file copy implementation copy-in must use. Greenlit's normal
    /// runtime leaves this at [`FileCopyPolicy::Auto`]; capability gates force
    /// one implementation so they fail unless that real path executes.
    pub file_copy: FileCopyPolicy,
    /// The job command to `exec(2)` once isolation is established: program name
    /// followed by its arguments. Always non-empty.
    pub command: Vec<String>,
}

/// The caller's requested isolation mechanism (`--strategy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrategyPref {
    /// Try overlay; fall back to copy-in when unprivileged overlayfs is
    /// unavailable. The default.
    #[default]
    Auto,
    /// Require overlay; fail loudly if it cannot be mounted. Used to prove the
    /// overlay path actually ran in an overlay-capable environment.
    Overlay,
    /// Skip overlay entirely and copy the checkout in. Used to exercise the
    /// fallback path everywhere, including CI runners without overlayfs.
    CopyIn,
}

/// The per-file implementation used while materializing a copy-in workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileCopyPolicy {
    /// Attempt a kernel reflink first, then use bounded streaming when the
    /// backing filesystem does not support `FICLONE`.
    #[default]
    Auto,
    /// Require the kernel `FICLONE` path and fail if it is unavailable.
    RequireReflink,
    /// Bypass `FICLONE` and copy through Greenlit's fixed-size userspace
    /// buffer.
    BoundedStream,
}

/// Everything that can go wrong while parsing the argument vector.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CliError {
    /// A recognized flag was given without its required value.
    #[error("missing value for {flag} — expected `{flag} <value>`")]
    MissingValue {
        /// The flag whose value was absent.
        flag: &'static str,
    },

    /// A required flag was never supplied.
    #[error("missing required argument {flag}")]
    MissingRequired {
        /// The flag that must be present.
        flag: &'static str,
    },

    /// An unrecognized token appeared before the `--` command separator.
    #[error(
        "unknown argument `{arg}` — expected --lower, --upper, --workspace, --strategy, --file-copy, or --"
    )]
    UnknownFlag {
        /// The offending token.
        arg: String,
    },

    /// `--strategy` was given a value outside the accepted set.
    #[error("invalid --strategy `{value}` — expected one of: auto, overlay, copy-in")]
    InvalidStrategy {
        /// The rejected value.
        value: String,
    },

    /// `--file-copy` was given a value outside the accepted set.
    #[error("invalid --file-copy `{value}` — expected one of: auto, reflink, bounded-stream")]
    InvalidFileCopy {
        /// The rejected value.
        value: String,
    },

    /// No command followed the `--` separator (or `--` was absent).
    #[error("no command to run — append `-- <program> [args...]` after the flags")]
    MissingCommand,
}

impl Args {
    /// Parse an argument vector (the process arguments **without** `argv[0]`).
    ///
    /// Flags may appear in any order; the first bare `--` ends flag parsing and
    /// everything after it is the job command verbatim.
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`] describing the exact problem and the expected
    /// form when a flag is unknown, a value is missing, `--strategy` is invalid,
    /// a required flag is absent, or no command follows `--`.
    pub fn parse<I>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut lower: Option<PathBuf> = None;
        let mut upper: Option<PathBuf> = None;
        let mut workspace: Option<PathBuf> = None;
        let mut strategy = StrategyPref::Auto;
        let mut file_copy = FileCopyPolicy::Auto;
        let mut command: Vec<String> = Vec::new();

        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--lower" => lower = Some(PathBuf::from(take_value(&mut it, "--lower")?)),
                "--upper" => upper = Some(PathBuf::from(take_value(&mut it, "--upper")?)),
                "--workspace" => {
                    workspace = Some(PathBuf::from(take_value(&mut it, "--workspace")?))
                }
                "--strategy" => strategy = parse_strategy(&take_value(&mut it, "--strategy")?)?,
                "--file-copy" => file_copy = parse_file_copy(&take_value(&mut it, "--file-copy")?)?,
                "--" => {
                    command.extend(it.by_ref());
                    break;
                }
                _ => return Err(CliError::UnknownFlag { arg }),
            }
        }

        let lower = lower.ok_or(CliError::MissingRequired { flag: "--lower" })?;
        let upper = upper.ok_or(CliError::MissingRequired { flag: "--upper" })?;
        let workspace = workspace.ok_or(CliError::MissingRequired {
            flag: "--workspace",
        })?;
        if command.is_empty() {
            return Err(CliError::MissingCommand);
        }

        Ok(Args {
            lower,
            upper,
            workspace,
            strategy,
            file_copy,
            command,
        })
    }
}

/// Consume the next token as a flag's value, or report it missing.
fn take_value<I>(it: &mut I, flag: &'static str) -> Result<String, CliError>
where
    I: Iterator<Item = String>,
{
    it.next().ok_or(CliError::MissingValue { flag })
}

/// Map a `--strategy` value onto [`StrategyPref`].
fn parse_strategy(value: &str) -> Result<StrategyPref, CliError> {
    match value {
        "auto" => Ok(StrategyPref::Auto),
        "overlay" => Ok(StrategyPref::Overlay),
        "copy-in" => Ok(StrategyPref::CopyIn),
        _ => Err(CliError::InvalidStrategy {
            value: value.to_string(),
        }),
    }
}

/// Map a `--file-copy` value onto [`FileCopyPolicy`].
fn parse_file_copy(value: &str) -> Result<FileCopyPolicy, CliError> {
    match value {
        "auto" => Ok(FileCopyPolicy::Auto),
        "reflink" => Ok(FileCopyPolicy::RequireReflink),
        "bounded-stream" => Ok(FileCopyPolicy::BoundedStream),
        _ => Err(CliError::InvalidFileCopy {
            value: value.to_string(),
        }),
    }
}
