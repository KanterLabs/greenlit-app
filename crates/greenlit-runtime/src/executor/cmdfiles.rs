//! Per-step workflow command files: `GITHUB_ENV`, `GITHUB_OUTPUT`,
//! `GITHUB_PATH`, `GITHUB_STEP_SUMMARY`.
//!
//! GitHub creates these four files fresh for each step, points the step's
//! environment at them, and reads them back after the step finishes, feeding
//! the results into the `env`/`steps.<id>.outputs` contexts of later steps
//! (`PHASE-2-execution.md` "Workflow command files"). The executor materializes
//! them inside the container, runs the step, then reads and parses them here.
//! The parsers themselves are the engine's
//! ([`greenlit_engine::execution::command_files`]); this module is the runtime
//! glue that writes and reads them across the container boundary.

use std::time::{SystemTime, UNIX_EPOCH};

use greenlit_engine::execution::command_files::{
    Assignment, EnvFileError, accept_step_summary, parse_env_file, parse_path_file,
};
use indexmap::IndexMap;

use crate::engine::{ContainerEngine, ExecSpec};
use crate::error::RuntimeError;
use crate::executor::context::CaptureSink;

/// The four command files plus the step script, as absolute container paths.
#[derive(Debug, Clone)]
pub(crate) struct CommandFilePaths {
    /// Directory holding this step's files.
    pub dir: String,
    /// `GITHUB_ENV`.
    pub env: String,
    /// `GITHUB_OUTPUT`.
    pub output: String,
    /// `GITHUB_PATH`.
    pub path: String,
    /// `GITHUB_STEP_SUMMARY`.
    pub summary: String,
    /// The step's script file (the shell's `{0}`).
    pub script: String,
    /// Where the step's exec wrapper records its own PID (see
    /// [`crate::executor::step`]'s timeout-termination path).
    pub pid: String,
}

impl CommandFilePaths {
    /// Compute the command-file paths for step `index` under `base`.
    pub fn new(base: &str, index: usize) -> Self {
        let dir = format!("{base}/step-{index}");
        CommandFilePaths {
            env: format!("{dir}/env"),
            output: format!("{dir}/output"),
            path: format!("{dir}/path"),
            summary: format!("{dir}/summary"),
            script: format!("{dir}/script"),
            pid: format!("{dir}/pid"),
            dir,
        }
    }
}

/// A parse failure while collecting a step's command files.
#[derive(Debug, thiserror::Error)]
pub enum CommandFileError {
    /// A container operation failed while writing or reading a file.
    #[error(transparent)]
    Engine(#[from] RuntimeError),
    /// `GITHUB_ENV` was malformed.
    #[error("GITHUB_ENV is malformed: {0}")]
    Env(#[source] EnvFileError),
    /// `GITHUB_OUTPUT` was malformed.
    #[error("GITHUB_OUTPUT is malformed: {0}")]
    Output(#[source] EnvFileError),
    /// Materializing the command files inside the container failed.
    #[error(
        "could not prepare a step's workspace inside the container (exit {exit_code})\n  fix: ensure the runner image provides a POSIX `sh`, `mkdir`, and `cat`"
    )]
    Prepare {
        /// The non-zero exit code of the preparing exec.
        exit_code: i64,
    },
}

/// The effects a step's command files have on later steps.
#[derive(Debug, Default)]
pub(crate) struct CommandFileEffects {
    /// `GITHUB_ENV` assignments, in file order (later wins).
    pub env: Vec<Assignment>,
    /// `GITHUB_PATH` additions, highest-priority first.
    pub path_additions: Vec<String>,
    /// `GITHUB_OUTPUT` assignments, in file order.
    pub outputs: Vec<Assignment>,
    /// Whether the step summary fit GitHub's per-step size cap.
    pub summary_within_limit: bool,
}

/// Create the step directory, truncate the four command files fresh, and write
/// the step `script`.
///
/// # Errors
///
/// Returns [`RuntimeError`] if the preparing exec fails.
pub(crate) async fn prepare(
    engine: &dyn ContainerEngine,
    container: &str,
    paths: &CommandFilePaths,
    script: &str,
) -> Result<(), CommandFileError> {
    let delimiter = heredoc_delimiter(script);
    // One exec builds the directory, truncates the four files, and writes the
    // script via a quoted heredoc so the script body is taken literally (no
    // shell expansion). The whole program is a single argv element, so no outer
    // shell escaping is required.
    let program = format!(
        "mkdir -p {dir} && : > {env} && : > {output} && : > {path} && : > {summary} && \
         : > {pid} && \
         cat > {script_path} <<'{delimiter}'\n{script}\n{delimiter}\n",
        dir = paths.dir,
        env = paths.env,
        output = paths.output,
        path = paths.path,
        summary = paths.summary,
        pid = paths.pid,
        script_path = paths.script,
    );
    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), program],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = CaptureSink::default();
    let output = engine.exec(container, &spec, &mut sink).await?;
    if output.exit_code != 0 {
        // A non-zero prep is an infrastructure failure, not a step failure.
        return Err(CommandFileError::Prepare {
            exit_code: output.exit_code,
        });
    }
    Ok(())
}

/// Widen this step's command files so a *sibling* container can write them.
///
/// Every other step kind writes its command files from inside the job
/// container, as the same user that [`prepare`] created them as. A Docker
/// action does not: it runs in a second container, as whatever user its own
/// image declares — frequently not root (`USER node` is a common last line
/// of an action's Dockerfile). Without this, such an action's very first
/// `>> "$GITHUB_OUTPUT"` fails with a permission error that has nothing to
/// do with the workflow.
///
/// The widened files sit on a run-scoped named volume that only this run's
/// own containers mount, and are removed with it
/// (`crate::executor::actions::docker_action` module docs), so this is a
/// per-step relaxation inside an already-isolated boundary rather than a
/// widening of anything the host can see.
///
/// # Errors
///
/// Returns [`CommandFileError`] if the exec fails or exits non-zero.
pub(crate) async fn open_to_sibling(
    engine: &dyn ContainerEngine,
    container: &str,
    paths: &CommandFilePaths,
) -> Result<(), CommandFileError> {
    let program = format!(
        "chmod 0777 {dir} && chmod 0666 {env} {output} {path} {summary}",
        dir = paths.dir,
        env = paths.env,
        output = paths.output,
        path = paths.path,
        summary = paths.summary,
    );
    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), program],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = CaptureSink::default();
    let output = engine.exec(container, &spec, &mut sink).await?;
    if output.exit_code != 0 {
        return Err(CommandFileError::Prepare {
            exit_code: output.exit_code,
        });
    }
    Ok(())
}

/// Fold a step's collected effects into the job's live state, returning the
/// step's own `outputs` map.
///
/// `GITHUB_ENV` assignments override the job's accumulation in file order;
/// `GITHUB_PATH` additions are prepended ahead of everything already there
/// (highest priority first, which is why this is a prepend and not a push).
/// Every step kind that has command files applies them identically — this is
/// that one rule, in one place.
pub(crate) fn apply_effects(
    effects: &CommandFileEffects,
    accumulated: &mut IndexMap<String, String>,
    path_additions: &mut Vec<String>,
) -> IndexMap<String, String> {
    for assignment in &effects.env {
        accumulated.insert(assignment.name.clone(), assignment.value.clone());
    }
    if !effects.path_additions.is_empty() {
        let mut merged = effects.path_additions.clone();
        merged.append(path_additions);
        *path_additions = merged;
    }
    effects
        .outputs
        .iter()
        .map(|assignment| (assignment.name.clone(), assignment.value.clone()))
        .collect()
}

/// Read and parse a step's four command files after it ran.
///
/// # Errors
///
/// Returns [`CommandFileError`] if a read fails or `GITHUB_ENV`/`GITHUB_OUTPUT`
/// is malformed.
pub(crate) async fn collect(
    engine: &dyn ContainerEngine,
    container: &str,
    paths: &CommandFilePaths,
) -> Result<CommandFileEffects, CommandFileError> {
    let env_text = read_file(engine, container, &paths.env).await?;
    let output_text = read_file(engine, container, &paths.output).await?;
    let path_text = read_file(engine, container, &paths.path).await?;
    let summary_text = read_file(engine, container, &paths.summary).await?;

    let env = parse_env_file(&env_text).map_err(CommandFileError::Env)?;
    let outputs = parse_env_file(&output_text).map_err(CommandFileError::Output)?;
    let path_additions = parse_path_file(&path_text);
    let summary_within_limit = accept_step_summary(&summary_text).is_some();

    Ok(CommandFileEffects {
        env,
        path_additions,
        outputs,
        summary_within_limit,
    })
}

/// `cat` one file out of the container, returning its contents.
async fn read_file(
    engine: &dyn ContainerEngine,
    container: &str,
    file: &str,
) -> Result<String, RuntimeError> {
    let spec = ExecSpec {
        cmd: vec!["cat".to_string(), file.to_string()],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = CaptureSink::default();
    engine.exec(container, &spec, &mut sink).await?;
    Ok(sink.text())
}

/// Pick a heredoc delimiter guaranteed not to appear as a line in `script`.
fn heredoc_delimiter(script: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut delimiter = format!("GREENLIT_SCRIPT_EOF_{nanos}");
    while script.lines().any(|line| line == delimiter) {
        delimiter.push('X');
    }
    delimiter
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_distinct_per_step() {
        let a = CommandFilePaths::new("/greenlit/cmd", 0);
        let b = CommandFilePaths::new("/greenlit/cmd", 1);
        assert_ne!(a.dir, b.dir);
        assert!(a.env.starts_with(&a.dir));
        assert!(a.script.ends_with("/script"));
    }

    #[test]
    fn delimiter_avoids_collision_with_script_lines() {
        // A pathological script that contains a plausible delimiter still gets a
        // non-colliding one.
        let script = "line\nGREENLIT_SCRIPT_EOF_0\nmore";
        let delimiter = heredoc_delimiter(script);
        assert!(!script.lines().any(|line| line == delimiter));
    }
}
