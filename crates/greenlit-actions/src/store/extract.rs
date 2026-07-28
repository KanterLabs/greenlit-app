//! Path-traversal-safe, size-bounded `tar` extraction for fetched action
//! tarballs.
//!
//! `PHASE-3-actions.md`'s constraints: "Repository content and remote
//! responses are UNTRUSTED... tarball extraction must be path-traversal-safe
//! (reject `..`/absolute entries; symlink entries must not escape the SHA
//! dir), size-bounded, and never follow symlinks out of the store." This is
//! a pure function over any [`Read`] so it can be tested with crafted
//! in-memory archives (`TESTING.md`: "No mocking our own crates" — there is
//! nothing to mock here, hostile bytes are hostile bytes whether they came
//! from the network or a test).

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use tar::EntryType;

/// A ceiling on total extracted bytes across every regular file in the
/// archive. Real action repositories are source code — a handful of
/// megabytes at most — so this is a generous safety bound against a
/// malicious or corrupt tarball, not a realistic expectation.
pub(crate) const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
/// A ceiling on the number of entries, guarding against an archive crafted
/// to exhaust inodes/memory with many tiny/empty entries.
pub(crate) const MAX_EXTRACTED_ENTRIES: usize = 100_000;

/// A tarball could not be safely extracted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ExtractError {
    /// Reading the archive's structure failed (corrupt/truncated tar).
    #[error("could not read tar archive: {0}")]
    Read(String),
    /// An entry's path is absolute, or escapes the extraction root via
    /// `..`.
    #[error("tar entry has an unsafe path: {0}")]
    UnsafePath(String),
    /// An entry's path shares no common top-level directory with the rest
    /// of the archive (GitHub's tarballs wrap every entry in one such
    /// directory; a tarball that doesn't is not one this crate trusts to
    /// unwrap correctly).
    #[error("tar entry '{0}' is outside the archive's single top-level directory")]
    InconsistentTopLevel(String),
    /// A symlink (or hardlink) entry's target would resolve outside the
    /// extraction root.
    #[error("tar entry '{0}' is a symlink that would escape the destination directory")]
    UnsafeLinkTarget(String),
    /// An unsupported entry type (device node, FIFO, hardlink, …) — real
    /// action source trees (themselves derived from Git, which cannot even
    /// represent these) never legitimately contain one.
    #[error("tar entry '{0}' has an unsupported type ({1:?})")]
    UnsupportedEntryType(String, EntryType),
    /// The archive exceeded [`MAX_EXTRACTED_BYTES`].
    #[error("tarball exceeds the {0}-byte extraction size limit")]
    TooLarge(u64),
    /// The archive exceeded [`MAX_EXTRACTED_ENTRIES`].
    #[error("tarball exceeds the {0}-entry extraction limit")]
    TooManyEntries(usize),
    /// Writing an extracted file/directory/symlink to `dest` failed.
    #[error("could not write extracted entry to {path}: {message}")]
    Write {
        /// The destination path that could not be written.
        path: PathBuf,
        /// The underlying I/O error's message.
        message: String,
    },
}

/// Extracts a gzip-decompressed tar stream into `dest`, stripping the
/// single common top-level directory GitHub's tarball endpoint wraps every
/// entry in (see the module docs for why this crate does not hardcode that
/// directory's exact name).
///
/// `dest` must already exist and be empty; the caller
/// ([`crate::store::ActionStore`]) is responsible for the atomic
/// fetch-then-rename this is one half of.
pub(crate) fn extract_tarball(reader: impl Read, dest: &Path) -> Result<(), ExtractError> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|error| ExtractError::Read(error.to_string()))?;

    let mut total_bytes: u64 = 0;
    let mut count: usize = 0;
    let mut top_level: Option<std::ffi::OsString> = None;

    for entry in entries {
        let mut entry = entry.map_err(|error| ExtractError::Read(error.to_string()))?;

        count += 1;
        if count > MAX_EXTRACTED_ENTRIES {
            return Err(ExtractError::TooManyEntries(MAX_EXTRACTED_ENTRIES));
        }
        total_bytes = total_bytes.saturating_add(entry.header().size().unwrap_or(0));
        if total_bytes > MAX_EXTRACTED_BYTES {
            return Err(ExtractError::TooLarge(MAX_EXTRACTED_BYTES));
        }

        let raw_path = entry
            .path()
            .map_err(|error| ExtractError::Read(error.to_string()))?
            .into_owned();
        let display_path = raw_path.display().to_string();

        let mut components = raw_path.components();
        let first = components
            .next()
            .ok_or_else(|| ExtractError::UnsafePath(display_path.clone()))?;
        let Component::Normal(first) = first else {
            return Err(ExtractError::UnsafePath(display_path));
        };
        match &top_level {
            None => top_level = Some(first.to_owned()),
            Some(expected) if expected == first => {}
            Some(_) => return Err(ExtractError::InconsistentTopLevel(display_path)),
        }

        let relative = safe_relative_path(components, &display_path)?;
        if relative.as_os_str().is_empty() {
            // The bare top-level directory entry itself; `dest` already is
            // its replacement.
            continue;
        }
        let target = dest.join(&relative);

        match entry.header().entry_type() {
            EntryType::Directory => {
                create_dir_all(&target)?;
            }
            EntryType::Regular => {
                if let Some(parent) = target.parent() {
                    create_dir_all(parent)?;
                }
                let mut file = File::create(&target).map_err(|error| ExtractError::Write {
                    path: target.clone(),
                    message: error.to_string(),
                })?;
                std::io::copy(&mut entry, &mut file).map_err(|error| ExtractError::Write {
                    path: target.clone(),
                    message: error.to_string(),
                })?;
                apply_mode(&file, &entry, &target)?;
            }
            EntryType::Symlink => {
                let link_name = entry
                    .link_name()
                    .map_err(|error| ExtractError::Read(error.to_string()))?
                    .ok_or_else(|| ExtractError::UnsafeLinkTarget(display_path.clone()))?;
                reject_escaping_link(&relative, &link_name, &display_path)?;
                if let Some(parent) = target.parent() {
                    create_dir_all(parent)?;
                }
                std::os::unix::fs::symlink(&link_name, &target).map_err(|error| {
                    ExtractError::Write {
                        path: target.clone(),
                        message: error.to_string(),
                    }
                })?;
            }
            other => return Err(ExtractError::UnsupportedEntryType(display_path, other)),
        }
    }
    Ok(())
}

fn create_dir_all(path: &Path) -> Result<(), ExtractError> {
    std::fs::create_dir_all(path).map_err(|error| ExtractError::Write {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

/// Sets the extracted file's Unix permission bits from the tar header
/// (masked to the standard `rwx` bits), so an action's executable
/// scripts/binaries keep their executable bit; falls back silently to
/// whatever [`File::create`] already produced when the header carries no
/// mode, since that is still a safe, usable file.
fn apply_mode(
    file: &File,
    entry: &tar::Entry<'_, impl Read>,
    target: &Path,
) -> Result<(), ExtractError> {
    use std::os::unix::fs::PermissionsExt;
    let Ok(mode) = entry.header().mode() else {
        return Ok(());
    };
    let permissions = std::fs::Permissions::from_mode(mode & 0o777);
    file.set_permissions(permissions)
        .map_err(|error| ExtractError::Write {
            path: target.to_path_buf(),
            message: error.to_string(),
        })
}

/// Strips `..`/root/prefix components from `components` (which no longer
/// include the already-consumed top-level directory), rejecting the entry
/// outright rather than silently normalizing a traversal attempt away.
fn safe_relative_path(
    components: std::path::Components<'_>,
    display_path: &str,
) -> Result<PathBuf, ExtractError> {
    let mut result = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ExtractError::UnsafePath(display_path.to_owned()));
            }
        }
    }
    Ok(result)
}

/// Rejects a symlink whose target would resolve outside the destination
/// directory, tracked purely at the path-component level (no entries have
/// been written to disk yet when this runs, so there is nothing for the
/// target to have already escaped through).
///
/// An absolute target is rejected outright. A relative target is walked
/// component-by-component from the symlink's own directory depth: each
/// `..` must be matched by an earlier descent, or the target would resolve
/// above the destination root.
fn reject_escaping_link(
    relative: &Path,
    link_name: &Path,
    display_path: &str,
) -> Result<(), ExtractError> {
    if link_name.is_absolute() {
        return Err(ExtractError::UnsafeLinkTarget(display_path.to_owned()));
    }
    // Depth of the directory the symlink itself lives in, i.e. the number
    // of path segments before its own file name.
    let mut depth: i64 =
        i64::try_from(relative.components().count().saturating_sub(1)).unwrap_or(i64::MAX);
    for component in link_name.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(ExtractError::UnsafeLinkTarget(display_path.to_owned()));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ExtractError::UnsafeLinkTarget(display_path.to_owned()));
            }
        }
    }
    Ok(())
}
