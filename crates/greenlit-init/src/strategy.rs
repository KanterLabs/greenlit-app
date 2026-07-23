//! Isolation-strategy decision logic — pure, root-free, and unit-tested.
//!
//! Nothing here touches the kernel or the filesystem: it decides *what* to do
//! from a preference plus the raw `errno` of a `mount(2)` attempt, and it
//! builds the overlay option string. The actual mount lives in [`crate::mount`]
//! and is covered by Phase 2 integration tests (which can run privileged),
//! keeping this decision surface testable without root.

use std::path::{Path, PathBuf};

/// Stderr marker printed when the helper falls back from overlay to copy-in.
///
/// A stable, greppable token (deliberately **not** a `::group::`-style GitHub
/// log command, so Greenlit's log-command parser never mistakes it for one) so
/// tooling and tests can detect that the fallback path ran. The full line is:
///
/// ```text
/// GREENLIT_INIT_FALLBACK strategy=copy-in reason=<ERRNO>: <explanation>
/// ```
pub const FALLBACK_MARKER: &str = "GREENLIT_INIT_FALLBACK";

/// The isolation mechanism chosen for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Isolation {
    /// The overlay mount succeeded; the merged view is live at the workspace.
    Overlay,
    /// The checkout is copied into the workspace instead.
    CopyIn {
        /// `true` when this was a fallback from a failed overlay attempt (which
        /// emits [`FALLBACK_MARKER`]); `false` when copy-in was requested up
        /// front via `--strategy copy-in`.
        fell_back: bool,
    },
}

/// How a failed overlay `mount(2)` should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountFailure {
    /// The environment does not permit or support unprivileged overlayfs here.
    /// Under `--strategy auto` this triggers the copy-in fallback.
    Unsupported,
    /// A genuine setup error (e.g. a missing directory) that must never be
    /// silently masked by falling back.
    Fatal,
}

/// A mount failure that must not be masked by falling back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecisionError {
    /// `--strategy overlay` was requested but the mount failed.
    OverlayRequired {
        /// The `errno` returned by `mount(2)`.
        errno: i32,
    },
    /// `--strategy auto` attempted overlay and hit an error that is not a
    /// "no overlay support here" condition, so falling back would hide a real
    /// misconfiguration.
    MountFatal {
        /// The `errno` returned by `mount(2)`.
        errno: i32,
    },
}

/// Classify a failed overlay `mount(2)` by its `errno`.
///
/// Unprivileged overlayfs surfaces "not available here" as a small, stable set
/// of errnos: `EPERM`/`EACCES` (mount not permitted for this user/namespace),
/// `ENODEV` (the `overlay` filesystem is not registered in the kernel),
/// `ENOSYS`/`EOPNOTSUPP` (the operation is unsupported), and `EINVAL` (the
/// kernel rejected an otherwise well-formed overlay mount — the common signal
/// that unprivileged overlay is disallowed, e.g. when `upperdir`/`workdir` are
/// on a filesystem the kernel refuses for an unprivileged overlay). Since the
/// helper always builds a well-formed option string itself, `EINVAL` here means
/// "overlay unusable in this environment", so it falls back rather than
/// aborting. Every other errno is treated as a real, non-maskable failure.
pub(crate) fn classify_mount_error(errno: i32) -> MountFailure {
    match errno {
        libc::EPERM
        | libc::EACCES
        | libc::ENODEV
        | libc::ENOSYS
        | libc::EOPNOTSUPP
        | libc::EINVAL => MountFailure::Unsupported,
        _ => MountFailure::Fatal,
    }
}

/// A short, stable name for the errnos this helper reasons about, for the
/// [`FALLBACK_MARKER`] line. Unrecognized errnos render as `E<number>`.
pub(crate) fn errno_name(errno: i32) -> String {
    match errno {
        libc::EPERM => "EPERM".to_string(),
        libc::EACCES => "EACCES".to_string(),
        libc::ENODEV => "ENODEV".to_string(),
        libc::ENOSYS => "ENOSYS".to_string(),
        libc::EOPNOTSUPP => "EOPNOTSUPP".to_string(),
        libc::EINVAL => "EINVAL".to_string(),
        other => format!("E{other}"),
    }
}

/// Decide the isolation outcome after an overlay mount was attempted.
///
/// `require_overlay` is `true` for `--strategy overlay`; `mount_errno` is
/// `None` on a successful mount and `Some(errno)` on failure. `--strategy
/// copy-in` never attempts a mount and so never reaches this function.
///
/// # Errors
///
/// Returns [`DecisionError::OverlayRequired`] when overlay was mandatory but
/// failed, or [`DecisionError::MountFatal`] when an `auto` attempt failed for a
/// reason that is not "overlay unavailable".
pub(crate) fn decide_after_overlay_attempt(
    require_overlay: bool,
    mount_errno: Option<i32>,
) -> Result<Isolation, DecisionError> {
    match mount_errno {
        None => Ok(Isolation::Overlay),
        Some(errno) => {
            if require_overlay {
                return Err(DecisionError::OverlayRequired { errno });
            }
            match classify_mount_error(errno) {
                MountFailure::Unsupported => Ok(Isolation::CopyIn { fell_back: true }),
                MountFailure::Fatal => Err(DecisionError::MountFatal { errno }),
            }
        }
    }
}

/// A path that cannot be represented in an overlayfs option string.
///
/// Kept separate from [`crate::error::InitError`] (which carries non-comparable
/// `io::Error` sources) so this pure logic stays `PartialEq`-testable; [`run`]
/// lifts it into an `InitError`.
///
/// [`run`]: crate::run
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathError {
    /// The path is not valid UTF-8 and so cannot be placed in the option
    /// string this helper builds.
    NonUtf8 {
        /// The offending path.
        path: PathBuf,
    },
    /// The path contains `,` or `:`, which overlayfs reads as option and
    /// lower-layer separators respectively.
    Separator {
        /// The offending path.
        path: PathBuf,
    },
}

/// Build the overlayfs `data` option string: `lowerdir=…,upperdir=…,workdir=…`.
///
/// # Errors
///
/// overlayfs separates options with `,` and lower layers with `:`, so neither
/// character can appear in a component path. A non-UTF-8 path or one containing
/// `,`/`:` yields a [`PathError`] rather than a silently corrupt mount request.
pub(crate) fn overlay_options(
    lower: &Path,
    upper: &Path,
    work: &Path,
) -> Result<String, PathError> {
    let lower = encode_component(lower)?;
    let upper = encode_component(upper)?;
    let work = encode_component(work)?;
    Ok(format!("lowerdir={lower},upperdir={upper},workdir={work}"))
}

/// Validate one overlay path component and return it as a `&str`.
fn encode_component(path: &Path) -> Result<&str, PathError> {
    let s = path.to_str().ok_or_else(|| PathError::NonUtf8 {
        path: path.to_path_buf(),
    })?;
    if s.contains(',') || s.contains(':') {
        return Err(PathError::Separator {
            path: path.to_path_buf(),
        });
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_errnos_are_unsupported() {
        for errno in [
            libc::EPERM,
            libc::EACCES,
            libc::ENODEV,
            libc::ENOSYS,
            libc::EOPNOTSUPP,
            libc::EINVAL,
        ] {
            assert_eq!(classify_mount_error(errno), MountFailure::Unsupported);
        }
    }

    #[test]
    fn other_errnos_are_fatal() {
        for errno in [libc::ENOENT, libc::ENOSPC, libc::EROFS] {
            assert_eq!(classify_mount_error(errno), MountFailure::Fatal);
        }
    }

    #[test]
    fn successful_mount_uses_overlay_regardless_of_preference() {
        assert_eq!(
            decide_after_overlay_attempt(false, None),
            Ok(Isolation::Overlay)
        );
        assert_eq!(
            decide_after_overlay_attempt(true, None),
            Ok(Isolation::Overlay)
        );
    }

    #[test]
    fn auto_falls_back_on_unsupported_and_marks_it() {
        assert_eq!(
            decide_after_overlay_attempt(false, Some(libc::EPERM)),
            Ok(Isolation::CopyIn { fell_back: true })
        );
    }

    #[test]
    fn auto_does_not_mask_a_fatal_failure() {
        assert_eq!(
            decide_after_overlay_attempt(false, Some(libc::ENOENT)),
            Err(DecisionError::MountFatal {
                errno: libc::ENOENT
            })
        );
    }

    #[test]
    fn required_overlay_never_falls_back() {
        // Even an "unsupported" errno is an error when overlay is mandatory.
        assert_eq!(
            decide_after_overlay_attempt(true, Some(libc::EPERM)),
            Err(DecisionError::OverlayRequired { errno: libc::EPERM })
        );
    }

    #[test]
    fn builds_the_overlay_option_string() {
        let opts = overlay_options(
            Path::new("/ro/repo"),
            Path::new("/tmp/up/upper"),
            Path::new("/tmp/up/work"),
        )
        .expect("plain paths");
        assert_eq!(
            opts,
            "lowerdir=/ro/repo,upperdir=/tmp/up/upper,workdir=/tmp/up/work"
        );
    }

    #[test]
    fn rejects_paths_that_break_option_syntax() {
        // A comma would be read as an option separator.
        assert_eq!(
            overlay_options(Path::new("/a,b"), Path::new("/up"), Path::new("/wk")),
            Err(PathError::Separator {
                path: PathBuf::from("/a,b")
            })
        );
        // A colon would be read as a lower-layer separator.
        assert_eq!(
            overlay_options(Path::new("/lo"), Path::new("/u:p"), Path::new("/wk")),
            Err(PathError::Separator {
                path: PathBuf::from("/u:p")
            })
        );
    }

    #[test]
    fn errno_names_cover_the_known_set_and_fall_through() {
        assert_eq!(errno_name(libc::EPERM), "EPERM");
        assert_eq!(errno_name(libc::EINVAL), "EINVAL");
        assert_eq!(errno_name(libc::EOPNOTSUPP), "EOPNOTSUPP");
        // An errno outside the known set renders numerically.
        assert_eq!(errno_name(libc::ENOENT), format!("E{}", libc::ENOENT));
    }
}
