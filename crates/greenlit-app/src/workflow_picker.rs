//! Interactive workflow selection for ambiguous discovery.
//!
//! When `.github/workflows/` holds several files and no `-W` was passed, an
//! interactive invocation prompts with an arrow-key selector instead of
//! erroring. Each row shows the file name, the workflow's `name:` (when the
//! file parses), and its `on:` triggers, so the pick is informed without
//! opening the files. The prompt renders on stderr — stdout stays reserved
//! for plan/run output — and only ever appears when both stdin and stderr
//! are terminals; scripts, CI, and `--no-input` runs keep the explicit
//! "pass `-W`" error unchanged.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;
use greenlit_workflow::model::trigger::Trigger;
use greenlit_workflow::model::workflow::Workflow;

use crate::render::terminal::inline_escape;

use crate::workflow_discovery::{self, ResolvedWorkflowPath, WorkflowResolution};

/// Whether this invocation may prompt at all: both stdin (the answer) and
/// stderr (the menu) must be terminals.
pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Completes a command's workflow resolution: an ambiguous discovery prompts
/// when the caller allows it (`allow_prompt` is false under `--no-input`)
/// and the terminal permits, and otherwise reports the prepared
/// non-interactive error unchanged.
pub(crate) fn resolve_or_pick(
    resolution: WorkflowResolution,
    repo_root: &Path,
    allow_prompt: bool,
) -> Result<ResolvedWorkflowPath, String> {
    match resolution {
        WorkflowResolution::Resolved(path) => Ok(path),
        WorkflowResolution::Ambiguous { candidates, error } => {
            if allow_prompt && interactive() {
                let picked = pick(&candidates, repo_root)?;
                workflow_discovery::resolve_picked(&picked, repo_root)
            } else {
                Err(error)
            }
        }
    }
}

/// Prompts for one of `candidates` (paths under the repository's workflows
/// directory) and returns the chosen path. A declined prompt (Esc/`q`) or a
/// terminal-level failure degrades to the same explicit `-W` fix the
/// non-interactive path reports.
pub(crate) fn pick(candidates: &[PathBuf], repo_root: &Path) -> Result<PathBuf, String> {
    let labels: Vec<String> = candidates
        .iter()
        .map(|path| label(path, repo_root))
        .collect();
    let picked = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Which workflow?")
        .items(&labels)
        .default(0)
        .interact_opt()
        .map_err(|error| {
            format!(
                "workflow selection failed: {error}\n  fix: pass -W <path> to select one explicitly",
                error = inline_escape(&error.to_string())
            )
        })?;
    match picked {
        Some(index) => Ok(candidates[index].clone()),
        None => Err(
            "workflow selection cancelled\n  fix: pass -W <path> to select one explicitly"
                .to_string(),
        ),
    }
}

/// One selector row: `<file> — <workflow name> (<triggers>)`. The file name
/// alone when the workflow has no `name:`; a `(does not parse)` marker when
/// the file is not a valid workflow (still selectable — the ordinary parse
/// error then reports the real problem with spans). Both the `name:` value
/// and the file name are repository-authored, so they are escaped before
/// reaching the terminal.
fn label(path: &Path, repo_root: &Path) -> String {
    let file = path
        .strip_prefix(repo_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let file = inline_escape(&file);
    match greenlit_workflow::parse_workflow_file_with_name(path, file.as_str()) {
        Ok(workflow) => {
            let triggers = trigger_summary(&workflow);
            match workflow.name {
                Some(name) => format!("{file} — {} ({triggers})", inline_escape(&name.value)),
                None => format!("{file} ({triggers})"),
            }
        }
        Err(_) => format!("{file} (does not parse)"),
    }
}

/// The workflow's trigger names, comma-joined in declaration order without
/// duplicates (e.g. `push, pull_request` or `workflow_dispatch`).
fn trigger_summary(workflow: &Workflow) -> String {
    let mut names: Vec<&str> = Vec::new();
    for trigger in &workflow.on {
        let name = match &trigger.value {
            Trigger::Webhook { name, .. } => name.as_str(),
            Trigger::WorkflowDispatch(_) => "workflow_dispatch",
            Trigger::Schedule(_) => "schedule",
            Trigger::RepositoryDispatch { .. } => "repository_dispatch",
            Trigger::WorkflowCall(_) => "workflow_call",
            Trigger::Other { name, .. } => name.as_str(),
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return "no triggers".to_string();
    }
    inline_escape(&names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write test workflow");
        path
    }

    #[test]
    fn label_shows_file_name_and_triggers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "ci.yml",
            "name: CI\non: [push, pull_request]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        );
        assert_eq!(label(&path, dir.path()), "ci.yml — CI (push, pull_request)");
    }

    #[test]
    fn label_without_a_workflow_name_still_shows_triggers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "dogfood.yml",
            "on: workflow_dispatch\njobs:\n  gates:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        );
        assert_eq!(label(&path, dir.path()), "dogfood.yml (workflow_dispatch)");
    }

    #[test]
    fn label_marks_an_unparseable_file_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "broken.yml", "on: [unclosed\n");
        assert_eq!(label(&path, dir.path()), "broken.yml (does not parse)");
    }

    #[test]
    fn label_escapes_a_repository_authored_workflow_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "sneaky.yml",
            "name: \"evil\\u001b[2Jname\"\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        );
        let rendered = label(&path, dir.path());
        assert!(
            !rendered.contains('\u{1b}'),
            "escape byte must not reach the terminal: {rendered:?}"
        );
    }
}
