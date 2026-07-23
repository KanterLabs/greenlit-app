//! Repository-contained, non-expanding `.litci/vars` loading.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, openat};
use rustix::io::Errno;

use super::validate_name;

/// Repository-local safety boundary for `.litci/vars`. GitHub limits the
/// variable payload exposed to a workflow much more tightly; one MiB leaves
/// room for dotenv syntax and comments while preventing a tracked file from
/// consuming unbounded host memory before planning starts.
const MAX_DOTENV_FILE_BYTES: usize = 1024 * 1024;
const MAX_DOTENV_ASSIGNMENTS: usize = 2_000;

fn safe(text: &str) -> String {
    crate::render::terminal::inline_escape(text)
}

fn safe_path(path: &Path) -> String {
    safe(&path.display().to_string())
}

enum ParsedLine {
    Skip,
    Entry(String, String),
    Incomplete,
}

#[derive(Clone, Copy)]
enum Quote {
    None,
    Single,
    Double,
}

struct ValueScan {
    quote: Quote,
    escaped: bool,
    expecting_end: bool,
    at_start: bool,
}

impl ValueScan {
    fn new() -> Self {
        Self {
            quote: Quote::None,
            escaped: false,
            expecting_end: false,
            at_start: true,
        }
    }

    fn completes_record(&mut self, input: &str) -> bool {
        for character in input.chars() {
            match self.quote {
                Quote::Single => {
                    if character == '\'' {
                        self.quote = Quote::None;
                    }
                }
                Quote::Double if self.escaped => self.escaped = false,
                Quote::Double => match character {
                    '"' => self.quote = Quote::None,
                    '\\' => self.escaped = true,
                    _ => {}
                },
                Quote::None if self.escaped => {
                    self.escaped = false;
                    self.at_start = false;
                }
                Quote::None if self.expecting_end => match character {
                    ' ' | '\t' | '\r' => {}
                    '#' => return true,
                    _ => return true,
                },
                Quote::None => match character {
                    '\'' => {
                        self.quote = Quote::Single;
                        self.at_start = false;
                    }
                    '"' => {
                        self.quote = Quote::Double;
                        self.at_start = false;
                    }
                    '\\' => self.escaped = true,
                    ' ' | '\t' | '\r' => self.expecting_end = true,
                    '#' if self.at_start => return true,
                    _ => self.at_start = false,
                },
            }
        }
        if self.escaped {
            return true;
        }
        matches!(self.quote, Quote::None)
    }
}

/// Reads `.litci/vars` as non-expanding dotenv-style `KEY=VALUE` pairs.
///
/// A missing file is empty history, not an error. The directory and file are
/// opened one component at a time relative to the repository directory with
/// no-follow semantics, so a repository-authored symlink cannot redirect a
/// read into a host file. `O_NONBLOCK` ensures a special file cannot stall
/// before its type is rejected. At most one MiB plus the boundary byte is
/// read, and no more than 2,000 assignments are retained, so a tracked file
/// cannot force unbounded parser or downstream context allocations. Values
/// retain dotenv quoting, escaping, comments, whitespace, and optional
/// `export` support, but `$NAME` and `${NAME}` remain literal and never read
/// Greenlit's process environment.
///
/// `None` means the file was absent; `Some(empty)` means an existing empty
/// file explicitly supplied a complete empty variable map.
pub(crate) fn read_dotenv_vars(repo_root: &Path) -> Result<Option<Vec<(String, String)>>, String> {
    let path = vars_path(repo_root);
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
        "vars",
        base_flags | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(component_error(&path, ".litci/vars", error)),
    };
    let metadata = file.metadata().map_err(|error| read_error(&path, &error))?;
    if !metadata.is_file() {
        return Err(read_error_message(
            &path,
            "local variables path is not a regular file",
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
            "{}: local variables file is not valid UTF-8\n  fix: save {} as UTF-8, then retry",
            safe_path(&path),
            safe_path(&path)
        )
    })?;
    parse(&path, source.strip_prefix('\u{feff}').unwrap_or(&source)).map(Some)
}

fn vars_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".litci").join("vars")
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
        "{}: could not read local variables file: {message}\n  fix: make {} a readable regular file inside the repository, then retry",
        safe_path(path),
        safe_path(path),
        message = safe(message)
    )
}

fn size_limit_error(path: &Path) -> String {
    format!(
        "{}: local variables file exceeds Greenlit's 1 MiB safety limit\n  fix: reduce {} to 1 MiB or less, then retry",
        safe_path(path),
        safe_path(path)
    )
}

fn assignment_limit_error(path: &Path) -> String {
    format!(
        "{}: local variables file exceeds Greenlit's 2,000-assignment safety limit\n  fix: reduce {} to 2,000 assignments or fewer, then retry",
        safe_path(path),
        safe_path(path)
    )
}

fn parse(path: &Path, source: &str) -> Result<Vec<(String, String)>, String> {
    let mut entries = Vec::new();
    let mut logical = String::new();
    let mut start_line = 1;
    let mut scan = ValueScan::new();
    for (index, raw_line) in source.split('\n').enumerate() {
        let physical = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if logical.is_empty() {
            start_line = index + 1;
            let trimmed = physical.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((_, input)) = physical.split_once('=') else {
                return Err(syntax_error(path, start_line));
            };
            scan = ValueScan::new();
            logical.push_str(physical);
            if !scan.completes_record(input.trim_start()) {
                continue;
            }
        } else {
            logical.push('\n');
            logical.push_str(physical);
            if !scan.completes_record(physical) {
                continue;
            }
        }
        match parse_line(path, start_line, &logical)? {
            ParsedLine::Skip => {}
            ParsedLine::Entry(key, value) => {
                if entries.len() == MAX_DOTENV_ASSIGNMENTS {
                    return Err(assignment_limit_error(path));
                }
                entries.push((key, value));
            }
            ParsedLine::Incomplete => return Err(syntax_error(path, start_line)),
        }
        logical.clear();
    }
    if !logical.is_empty() {
        return Err(syntax_error(path, start_line));
    }
    Ok(entries)
}

fn parse_line(path: &Path, line_number: usize, line: &str) -> Result<ParsedLine, String> {
    let mut assignment = line.trim_start();
    if assignment.is_empty() || assignment.starts_with('#') {
        return Ok(ParsedLine::Skip);
    }
    if let Some(rest) = assignment.strip_prefix("export")
        && rest.chars().next().is_some_and(char::is_whitespace)
    {
        assignment = rest.trim_start();
    }
    let Some((key, input)) = assignment.split_once('=') else {
        return Err(syntax_error(path, line_number));
    };
    let key = key.trim();
    if let Err(reason) = validate_name(key) {
        return Err(invalid_name(path, line_number, key, reason));
    }
    match parse_value(input.trim_start()).map_err(|()| syntax_error(path, line_number))? {
        Some(value) => Ok(ParsedLine::Entry(key.to_string(), value)),
        None => Ok(ParsedLine::Incomplete),
    }
}

fn parse_value(input: &str) -> Result<Option<String>, ()> {
    if input.is_empty() || input.starts_with('#') {
        return Ok(Some(String::new()));
    }
    let mut output = String::new();
    let mut quote = Quote::None;
    let mut expecting_end = false;
    let mut chars = input.chars();
    while let Some(character) = chars.next() {
        match quote {
            Quote::Single => {
                if character == '\'' {
                    quote = Quote::None;
                } else {
                    output.push(character);
                }
            }
            Quote::Double => match character {
                '"' => quote = Quote::None,
                '\\' => decode_escape(&mut chars, &mut output)?,
                _ => output.push(character),
            },
            Quote::None if expecting_end => match character {
                ' ' | '\t' | '\r' => {}
                '#' => return Ok(Some(output)),
                _ => return Err(()),
            },
            Quote::None => match character {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => decode_escape(&mut chars, &mut output)?,
                ' ' | '\t' | '\r' => expecting_end = true,
                _ => output.push(character),
            },
        }
    }
    match quote {
        Quote::None => Ok(Some(output)),
        Quote::Single | Quote::Double => Ok(None),
    }
}

fn decode_escape(chars: &mut impl Iterator<Item = char>, output: &mut String) -> Result<(), ()> {
    let Some(escaped) = chars.next() else {
        return Err(());
    };
    match escaped {
        '\\' | '\'' | '"' | '$' | ' ' => output.push(escaped),
        'n' => output.push('\n'),
        _ => return Err(()),
    }
    Ok(())
}

fn syntax_error(path: &Path, line: usize) -> String {
    format!(
        "{}:{line}: could not parse local variables file as KEY=VALUE\n  fix: correct the dotenv syntax near line {line}, then retry",
        safe_path(path)
    )
}

fn invalid_name(path: &Path, line: usize, name: &str, reason: &str) -> String {
    format!(
        "{}:{line}: invalid configuration-variable name '{name}': {reason}\n  fix: rename the key in {} to use only letters, digits, and underscores, starting with a letter or underscore and not GITHUB_",
        safe_path(path),
        safe_path(path),
        name = safe(name),
        reason = safe(reason)
    )
}
