//! Repository `.litci/secrets` reading, appending, and `.gitignore` upkeep.
//!
//! Reading follows the exact same no-follow, size/assignment-bounded
//! pattern as `.litci/vars` (`crate::vars::dotenv`) — see that module's doc
//! comment for the full security rationale, which applies identically here
//! (`.litci/secrets` is just as repository-adjacent and just as much a
//! target for a symlink-based host-file read). `PHASE-3-actions.md`
//! Secrets: "`.litci/secrets` (dotenv; create mode 0600; append `.litci/`
//! to `.gitignore` if missing)".

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, openat};
use rustix::io::Errno;

use super::validate_name;
use crate::dotenv_format::{DotenvError, parse_dotenv};

/// Mirrors `crate::vars::dotenv`'s identical boundary for `.litci/vars`.
const MAX_DOTENV_FILE_BYTES: usize = 1024 * 1024;
const MAX_DOTENV_ASSIGNMENTS: usize = 2_000;

fn safe(text: &str) -> String {
    crate::render::terminal::inline_escape(text)
}

fn safe_path(path: &Path) -> String {
    safe(&path.display().to_string())
}

fn secrets_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".litci").join("secrets")
}

/// Reads `.litci/secrets` the same way `.litci/vars` is read. `None` means
/// the file is absent (no local secret overrides at all).
pub(crate) fn read_dotenv_secrets(
    repo_root: &Path,
) -> Result<Option<Vec<(String, String)>>, String> {
    let path = secrets_path(repo_root);
    let repo_dir = File::open(repo_root).map_err(|error| read_error(&path, &error))?;
    if !repo_dir
        .metadata()
        .map_err(|error| read_error(&path, &error))?
        .is_dir()
    {
        return Err(read_error_message(
            &path,
            "repository root is not a directory",
        ));
    }

    let base_flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let litci_dir = match openat(
        &repo_dir,
        ".litci",
        base_flags | OFlags::DIRECTORY,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(component_error(&path, ".litci", error)),
    };
    let mut file = match openat(
        &litci_dir,
        "secrets",
        base_flags | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(component_error(&path, ".litci/secrets", error)),
    };
    let metadata = file.metadata().map_err(|error| read_error(&path, &error))?;
    if !metadata.is_file() {
        return Err(read_error_message(
            &path,
            "local secrets path is not a regular file",
        ));
    }
    if metadata.len() > MAX_DOTENV_FILE_BYTES as u64 {
        return Err(size_limit_error(&path));
    }

    let read_limit = u64::try_from(MAX_DOTENV_FILE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(MAX_DOTENV_FILE_BYTES.saturating_add(1));
    (&mut file)
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| read_error(&path, &error))?;
    if bytes.len() > MAX_DOTENV_FILE_BYTES {
        return Err(size_limit_error(&path));
    }
    let source = String::from_utf8(bytes).map_err(|_| {
        format!(
            "{}: local secrets file is not valid UTF-8\n  fix: save {} as UTF-8, then retry",
            safe_path(&path),
            safe_path(&path)
        )
    })?;
    let parsed = parse_dotenv(
        source.strip_prefix('\u{feff}').unwrap_or(&source),
        validate_name,
        MAX_DOTENV_ASSIGNMENTS,
    )
    .map_err(|error| match error {
        DotenvError::Syntax { line } => syntax_error(&path, line),
        DotenvError::InvalidName { line, name, reason } => invalid_name(&path, line, &name, reason),
        DotenvError::AssignmentLimit => assignment_limit_error(&path),
    })?;
    Ok(Some(parsed))
}

/// Appends one `NAME="value"` assignment to `.litci/secrets`, creating the
/// file at mode `0600` (and the `.litci/` directory) if it does not already
/// exist, and ensures `.litci/` is listed in the repository's `.gitignore`
/// (appending a `.litci/` line if the file exists but does not already
/// mention it, creating the file if it is entirely absent).
pub(crate) fn append_secret(repo_root: &Path, name: &str, value: &str) -> Result<(), String> {
    let repo_dir = File::open(repo_root)
        .map_err(|error| format!("could not open the repository root: {error}"))?;
    let litci_dir = match openat(
        &repo_dir,
        ".litci",
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => {
            rustix::fs::mkdirat(&repo_dir, ".litci", Mode::RUSR | Mode::WUSR | Mode::XUSR)
                .map_err(|error| format!("could not create .litci/: {error}"))?;
            let fd = openat(
                &repo_dir,
                ".litci",
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
                Mode::empty(),
            )
            .map_err(|error| format!("could not open .litci/ after creating it: {error}"))?;
            File::from(fd)
        }
        Err(error) => return Err(format!("could not open .litci/: {error}")),
    };
    let mode = Mode::RUSR | Mode::WUSR;
    let fd = openat(
        &litci_dir,
        "secrets",
        OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        mode,
    )
    .map_err(|error| format!("could not open .litci/secrets: {error}"))?;
    let mut file = File::from(fd);
    rustix::fs::fchmod(&file, mode).ok();
    let line = format!("{name}={}\n", escape_value(value));
    file.write_all(line.as_bytes())
        .map_err(|error| format!("could not append to .litci/secrets: {error}"))?;
    ensure_gitignored(&repo_dir)
}

/// Double-quotes `value` for a `.litci/secrets` line, escaping exactly the
/// characters `crate::dotenv_format::parse_dotenv`'s double-quote decoder
/// recognizes (`\`, `"`, and a literal newline as the two-character `\n`
/// escape) so a value containing any of them round-trips through a later
/// read unchanged.
fn escape_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

/// Ensures the repository's `.gitignore` lists `.litci/`, appending it (or
/// creating the file) if not already present. Best-effort: a workflow
/// author's `.gitignore` may itself already cover `.litci/` via a broader
/// pattern (e.g. a bare `.litci`), in which case an exact-line search would
/// miss it and this appends a redundant-but-harmless second line rather
/// than trying to parse gitignore glob semantics.
fn ensure_gitignored(repo_dir: &File) -> Result<(), String> {
    const LINE: &str = ".litci/\n";
    let existing = match openat(
        repo_dir,
        ".gitignore",
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let mut file = File::from(fd);
            let mut contents = String::new();
            file.read_to_string(&mut contents)
                .map_err(|error| format!("could not read .gitignore: {error}"))?;
            Some(contents)
        }
        Err(Errno::NOENT) => None,
        Err(error) => return Err(format!("could not open .gitignore: {error}")),
    };
    if let Some(contents) = &existing
        && contents
            .lines()
            .any(|line| line.trim() == ".litci/" || line.trim() == ".litci")
    {
        return Ok(());
    }
    let fd = openat(
        repo_dir,
        ".gitignore",
        OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
    )
    .map_err(|error| format!("could not open .gitignore for writing: {error}"))?;
    let mut file = File::from(fd);
    let needs_leading_newline =
        existing.is_some_and(|contents| !contents.ends_with('\n') && !contents.is_empty());
    let line = if needs_leading_newline {
        format!("\n{LINE}")
    } else {
        LINE.to_string()
    };
    file.write_all(line.as_bytes())
        .map_err(|error| format!("could not append to .gitignore: {error}"))
}

fn component_error(path: &Path, component: &str, error: Errno) -> String {
    if matches!(error, Errno::LOOP | Errno::NOTDIR) {
        return format!(
            "{}: {component} must be a real directory or regular file inside the repository, not a symbolic link\n  fix: replace {component} with a repository-local directory or regular file, then retry",
            safe_path(path)
        );
    }
    read_error_message(path, &error.to_string())
}

fn read_error(path: &Path, error: &std::io::Error) -> String {
    read_error_message(path, &error.to_string())
}

fn read_error_message(path: &Path, message: &str) -> String {
    format!(
        "{}: could not read local secrets file: {message}\n  fix: make {} a readable regular file inside the repository, then retry",
        safe_path(path),
        safe_path(path),
        message = safe(message)
    )
}

fn size_limit_error(path: &Path) -> String {
    format!(
        "{}: local secrets file exceeds Greenlit's 1 MiB safety limit\n  fix: reduce {} to 1 MiB or less, then retry",
        safe_path(path),
        safe_path(path)
    )
}

fn assignment_limit_error(path: &Path) -> String {
    format!(
        "{}: local secrets file exceeds Greenlit's 2,000-assignment safety limit\n  fix: reduce {} to 2,000 assignments or fewer, then retry",
        safe_path(path),
        safe_path(path)
    )
}

fn syntax_error(path: &Path, line: usize) -> String {
    format!(
        "{}:{line}: could not parse local secrets file as KEY=VALUE\n  fix: correct the dotenv syntax near line {line}, then retry",
        safe_path(path)
    )
}

fn invalid_name(path: &Path, line: usize, name: &str, reason: &str) -> String {
    format!(
        "{}:{line}: invalid secret name '{name}': {reason}\n  fix: rename the key in {} to use only letters, digits, and underscores, starting with a letter or underscore and not GITHUB_",
        safe_path(path),
        safe_path(path),
        name = safe(name),
        reason = safe(reason)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn append_secret_creates_the_file_at_mode_0600_and_gitignores_litci() {
        let repo = tempfile::tempdir().expect("tempdir");
        append_secret(repo.path(), "API_TOKEN", "s3cr3t").expect("append");

        let read_back = read_dotenv_secrets(repo.path())
            .expect("read")
            .expect("file exists");
        assert_eq!(
            read_back,
            vec![("API_TOKEN".to_string(), "s3cr3t".to_string())]
        );

        let metadata = std::fs::metadata(repo.path().join(".litci").join("secrets"))
            .expect("stat secrets file");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

        let gitignore =
            std::fs::read_to_string(repo.path().join(".gitignore")).expect("read .gitignore");
        assert!(gitignore.lines().any(|line| line.trim() == ".litci/"));
    }

    #[test]
    fn append_secret_does_not_duplicate_an_existing_gitignore_entry() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join(".gitignore"), "node_modules/\n.litci/\n")
            .expect("write .gitignore");
        append_secret(repo.path(), "API_TOKEN", "s3cr3t").expect("append");

        let gitignore =
            std::fs::read_to_string(repo.path().join(".gitignore")).expect("read .gitignore");
        assert_eq!(
            gitignore
                .lines()
                .filter(|line| line.trim() == ".litci/")
                .count(),
            1,
            "{gitignore}"
        );
        assert!(gitignore.contains("node_modules/"));
    }

    #[test]
    fn append_secret_escapes_special_characters_and_round_trips() {
        let repo = tempfile::tempdir().expect("tempdir");
        let tricky = "line1\nline2 with \"quotes\" and a \\backslash";
        append_secret(repo.path(), "MULTI", tricky).expect("append");

        let read_back = read_dotenv_secrets(repo.path())
            .expect("read")
            .expect("file exists");
        assert_eq!(read_back, vec![("MULTI".to_string(), tricky.to_string())]);
    }

    #[test]
    fn a_missing_secrets_file_reads_as_none() {
        let repo = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_dotenv_secrets(repo.path()).expect("read"), None);
    }
}
