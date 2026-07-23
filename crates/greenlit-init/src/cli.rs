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
    #[error("unknown argument `{arg}` — expected --lower, --upper, --workspace, --strategy, or --")]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_a_full_invocation() {
        let args = Args::parse(argv(&[
            "--lower",
            "/ro/repo",
            "--upper",
            "/tmp/up",
            "--workspace",
            "/gh/workspace",
            "--strategy",
            "overlay",
            "--",
            "bash",
            "-c",
            "echo hi",
        ]))
        .expect("valid invocation");

        assert_eq!(args.lower, PathBuf::from("/ro/repo"));
        assert_eq!(args.upper, PathBuf::from("/tmp/up"));
        assert_eq!(args.workspace, PathBuf::from("/gh/workspace"));
        assert_eq!(args.strategy, StrategyPref::Overlay);
        assert_eq!(args.command, vec!["bash", "-c", "echo hi"]);
    }

    #[test]
    fn strategy_defaults_to_auto_and_flags_may_reorder() {
        let args = Args::parse(argv(&[
            "--workspace",
            "/ws",
            "--lower",
            "/lo",
            "--upper",
            "/up",
            "--",
            "true",
        ]))
        .expect("valid invocation");
        assert_eq!(args.strategy, StrategyPref::Auto);
    }

    #[test]
    fn command_after_separator_is_verbatim_including_dashes() {
        // A `--` inside the command is part of the command, not a second
        // separator, and flag-shaped command tokens are not reinterpreted.
        let args = Args::parse(argv(&[
            "--lower",
            "/lo",
            "--upper",
            "/up",
            "--workspace",
            "/ws",
            "--",
            "sh",
            "-c",
            "--",
            "--lower",
        ]))
        .expect("valid invocation");
        assert_eq!(args.command, vec!["sh", "-c", "--", "--lower"]);
    }

    #[test]
    fn each_strategy_value_parses() {
        for (value, expected) in [
            ("auto", StrategyPref::Auto),
            ("overlay", StrategyPref::Overlay),
            ("copy-in", StrategyPref::CopyIn),
        ] {
            assert_eq!(parse_strategy(value), Ok(expected));
        }
    }

    #[test]
    fn rejects_unknown_strategy() {
        let err = Args::parse(argv(&[
            "--lower",
            "/lo",
            "--upper",
            "/up",
            "--workspace",
            "/ws",
            "--strategy",
            "bind",
            "--",
            "true",
        ]))
        .unwrap_err();
        assert_eq!(
            err,
            CliError::InvalidStrategy {
                value: "bind".to_string()
            }
        );
    }

    #[test]
    fn rejects_unknown_flag_before_separator() {
        let err = Args::parse(argv(&["--lower", "/lo", "--bogus"])).unwrap_err();
        assert_eq!(
            err,
            CliError::UnknownFlag {
                arg: "--bogus".to_string()
            }
        );
    }

    #[test]
    fn reports_missing_value() {
        let err = Args::parse(argv(&["--lower"])).unwrap_err();
        assert_eq!(err, CliError::MissingValue { flag: "--lower" });
    }

    #[test]
    fn reports_each_missing_required_flag() {
        // Missing --lower.
        let err = Args::parse(argv(&[
            "--upper",
            "/up",
            "--workspace",
            "/ws",
            "--",
            "true",
        ]))
        .unwrap_err();
        assert_eq!(err, CliError::MissingRequired { flag: "--lower" });
        // Missing --upper.
        let err = Args::parse(argv(&[
            "--lower",
            "/lo",
            "--workspace",
            "/ws",
            "--",
            "true",
        ]))
        .unwrap_err();
        assert_eq!(err, CliError::MissingRequired { flag: "--upper" });
        // Missing --workspace.
        let err =
            Args::parse(argv(&["--lower", "/lo", "--upper", "/up", "--", "true"])).unwrap_err();
        assert_eq!(
            err,
            CliError::MissingRequired {
                flag: "--workspace"
            }
        );
    }

    #[test]
    fn requires_a_command_after_separator() {
        // Separator present but empty.
        let err = Args::parse(argv(&[
            "--lower",
            "/lo",
            "--upper",
            "/up",
            "--workspace",
            "/ws",
            "--",
        ]))
        .unwrap_err();
        assert_eq!(err, CliError::MissingCommand);

        // No separator at all.
        let err = Args::parse(argv(&[
            "--lower",
            "/lo",
            "--upper",
            "/up",
            "--workspace",
            "/ws",
        ]))
        .unwrap_err();
        assert_eq!(err, CliError::MissingCommand);
    }
}
