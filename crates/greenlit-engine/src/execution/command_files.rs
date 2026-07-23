//! Parsers for GitHub's per-step *environment files*: `GITHUB_ENV`,
//! `GITHUB_OUTPUT`, `GITHUB_PATH`, and `GITHUB_STEP_SUMMARY`.
//!
//! Each file is created fresh for every step and read after the step
//! finishes. `GITHUB_ENV` and `GITHUB_OUTPUT` share one line format:
//! `NAME=VALUE`, or a heredoc block for multiline values:
//!
//! ```text
//! NAME<<DELIMITER
//! line one
//! line two
//! DELIMITER
//! ```
//!
//! `GITHUB_PATH` is one directory per line, prepended to `PATH`.
//! `GITHUB_STEP_SUMMARY` is opaque Markdown captured verbatim.
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands#environment-files>

use thiserror::Error;

/// The maximum size GitHub accepts for one step's `GITHUB_STEP_SUMMARY`:
/// 1 MiB. Larger content is rejected and the summary omitted.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands#adding-a-job-summary>
pub const MAX_STEP_SUMMARY_BYTES: usize = 1024 * 1024;

/// A malformed environment-file line or block.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnvFileError {
    /// A line was neither `NAME=VALUE` nor a heredoc opener.
    #[error(
        "invalid environment-file line (expected 'NAME=VALUE' or a 'NAME<<DELIMITER' block): {line}"
    )]
    InvalidLine {
        /// The offending line.
        line: String,
    },
    /// A heredoc block opened but its closing delimiter never appeared.
    #[error("unterminated heredoc block for '{name}' (missing closing delimiter '{delimiter}')")]
    UnterminatedHeredoc {
        /// The variable/output name.
        name: String,
        /// The delimiter the block was waiting for.
        delimiter: String,
    },
    /// A heredoc opener had an empty delimiter (`NAME<<`).
    #[error("heredoc opener for '{name}' has an empty delimiter")]
    EmptyDelimiter {
        /// The variable/output name.
        name: String,
    },
}

/// One parsed assignment from a `GITHUB_ENV`/`GITHUB_OUTPUT` file, in file
/// order. Later assignments to the same name override earlier ones when the
/// caller folds these into a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// The variable or output name.
    pub name: String,
    /// The assigned value (heredoc bodies keep their internal newlines but not
    /// a trailing newline).
    pub value: String,
}

/// Parses the shared `GITHUB_ENV`/`GITHUB_OUTPUT` format into ordered
/// assignments.
pub fn parse_env_file(contents: &str) -> Result<Vec<Assignment>, EnvFileError> {
    let mut assignments = Vec::new();
    // `lines()` drops the line terminators, which is exactly what the heredoc
    // body needs (join with `\n`, no trailing newline). A trailing empty line
    // from a final newline is naturally ignored.
    let mut lines = contents.lines().peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        if let Some((name, delimiter)) = heredoc_opener(line) {
            if delimiter.is_empty() {
                return Err(EnvFileError::EmptyDelimiter {
                    name: name.to_string(),
                });
            }
            let mut body: Vec<&str> = Vec::new();
            let mut closed = false;
            for body_line in lines.by_ref() {
                if body_line == delimiter {
                    closed = true;
                    break;
                }
                body.push(body_line);
            }
            if !closed {
                return Err(EnvFileError::UnterminatedHeredoc {
                    name: name.to_string(),
                    delimiter: delimiter.to_string(),
                });
            }
            assignments.push(Assignment {
                name: name.to_string(),
                value: body.join("\n"),
            });
        } else if let Some((name, value)) = line.split_once('=') {
            assignments.push(Assignment {
                name: name.to_string(),
                value: value.to_string(),
            });
        } else {
            return Err(EnvFileError::InvalidLine {
                line: line.to_string(),
            });
        }
    }
    Ok(assignments)
}

/// Recognizes a heredoc opener `NAME<<DELIMITER`. Returns `None` when the text
/// before `<<` contains an `=`, so an ordinary `NAME=value<<x` assignment is
/// not mistaken for a heredoc.
fn heredoc_opener(line: &str) -> Option<(&str, &str)> {
    let index = line.find("<<")?;
    let name = &line[..index];
    if name.contains('=') {
        return None;
    }
    Some((name, &line[index + 2..]))
}

/// Parses a `GITHUB_PATH` file: one directory per line, in file order. The
/// caller prepends these to `PATH` (first line becomes the highest-priority
/// entry). Blank lines are ignored.
pub fn parse_path_file(contents: &str) -> Vec<String> {
    contents
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Validates a `GITHUB_STEP_SUMMARY` payload against GitHub's per-step size
/// cap, returning the content when it fits. Content exceeding
/// [`MAX_STEP_SUMMARY_BYTES`] is rejected (GitHub drops the oversized
/// summary).
pub fn accept_step_summary(contents: &str) -> Option<&str> {
    if contents.len() > MAX_STEP_SUMMARY_BYTES {
        None
    } else {
        Some(contents)
    }
}
