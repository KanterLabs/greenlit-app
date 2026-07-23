//! Resolves which workflow file `litci plan` should read when `-W`/
//! `--workflow` is omitted: exactly one `*.yml`/`*.yaml` file directly under
//! `.github/workflows/`, matching GitHub's own workflow-file location
//! convention. Ambiguous or absent discovery is reported with the fix of
//! passing `-W` explicitly -- never guessed.

use std::path::{Path, PathBuf};

const WORKFLOWS_DIR: &str = ".github/workflows";
const MAX_REPORTED_CANDIDATES: usize = 20;

fn safe_path(path: &Path) -> String {
    crate::render::terminal::inline_escape(&path.display().to_string())
}

fn safe_error(error: &std::io::Error) -> String {
    crate::render::terminal::inline_escape(&error.to_string())
}

/// One workflow path with separate filesystem and user-facing forms. Reads
/// always use the canonical absolute path; source spans use the stable
/// repository-relative name GitHub exposes through `github.workflow` and
/// `github.workflow_ref`.
pub(crate) struct ResolvedWorkflowPath {
    pub(crate) read_path: PathBuf,
    pub(crate) source_name: String,
}

/// The outcome of workflow resolution. Discovery that finds several files is
/// not an error by itself: an interactive caller may prompt the user to pick
/// one (see `crate::workflow_picker`), while a non-interactive caller reports
/// the prepared `error` unchanged.
pub(crate) enum WorkflowResolution {
    /// Exactly one workflow was named or discovered.
    Resolved(ResolvedWorkflowPath),
    /// Several candidates exist under [`WORKFLOWS_DIR`] and none was named.
    Ambiguous {
        /// The discovered files, sorted, ready for an interactive picker.
        candidates: Vec<PathBuf>,
        /// The non-interactive report (message + `-W` fix action).
        error: String,
    },
}

/// Resolves the workflow file to plan: an explicit relative path is resolved
/// from `invocation_cwd`; otherwise the sole `*.yml`/`*.yaml` file under
/// [`WORKFLOWS_DIR`] in `repo_root` is selected. A workflow outside the
/// repository is rejected because it has no truthful repository-relative
/// identity in GitHub's context.
pub(crate) fn resolve_workflow_path(
    explicit: Option<&Path>,
    invocation_cwd: &Path,
    repo_root: &Path,
) -> Result<WorkflowResolution, String> {
    let selected = if let Some(path) = explicit {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            invocation_cwd.join(path)
        }
    } else {
        match discover_workflow(repo_root)? {
            Discovered::One(path) => path,
            Discovered::Several { candidates, error } => {
                return Ok(WorkflowResolution::Ambiguous { candidates, error });
            }
        }
    };
    normalize_workflow_path(&selected, repo_root).map(WorkflowResolution::Resolved)
}

/// Resolves one picked candidate exactly as an explicit `-W` path would be —
/// the interactive picker's follow-up to [`WorkflowResolution::Ambiguous`].
pub(crate) fn resolve_picked(
    picked: &Path,
    repo_root: &Path,
) -> Result<ResolvedWorkflowPath, String> {
    normalize_workflow_path(picked, repo_root)
}

enum Discovered {
    One(PathBuf),
    Several {
        candidates: Vec<PathBuf>,
        error: String,
    },
}

fn discover_workflow(repo_root: &Path) -> Result<Discovered, String> {
    let dir = repo_root.join(WORKFLOWS_DIR);
    let (mut candidates, additional_candidates) = discover_candidates(&dir)?;
    candidates.sort();
    match candidates.len() {
        0 => Err(format!(
            "no workflow file found under {}\n  fix: pass -W <path> to select a workflow file, or add one under {WORKFLOWS_DIR}",
            safe_path(&dir)
        )),
        1 => Ok(Discovered::One(candidates.remove(0))),
        _ => {
            let mut list = candidates
                .iter()
                .map(|path| safe_path(path))
                .collect::<Vec<_>>()
                .join(", ");
            if additional_candidates {
                list.push_str(", ... (additional candidates omitted)");
            }
            let error = format!(
                "multiple workflow files found under {}: {list}\n  fix: pass -W <path> to select one explicitly",
                safe_path(&dir)
            );
            // Beyond the reporting cap the list itself is untrustworthy as a
            // menu; fall back to the explicit `-W` error unconditionally.
            if additional_candidates {
                return Err(error);
            }
            Ok(Discovered::Several { candidates, error })
        }
    }
}

fn normalize_workflow_path(path: &Path, repo_root: &Path) -> Result<ResolvedWorkflowPath, String> {
    let read_path = std::fs::canonicalize(path).map_err(|error| {
        format!(
            "{}: could not resolve workflow file: {error}\n  fix: pass -W <path> naming a readable workflow file inside {}",
            safe_path(path),
            safe_path(repo_root),
            error = safe_error(&error)
        )
    })?;
    if !read_path.is_file() {
        return Err(format!(
            "{}: workflow path is not a file\n  fix: pass -W <path> naming a readable workflow file inside {}",
            safe_path(path),
            safe_path(repo_root)
        ));
    }
    let relative = read_path.strip_prefix(repo_root).map_err(|_| {
        format!(
            "{}: workflow file resolves outside repository {}\n  fix: pass -W <path> naming a workflow file inside the repository",
            safe_path(path),
            safe_path(repo_root)
        )
    })?;
    let source_name = relative.to_string_lossy().replace('\\', "/");
    Ok(ResolvedWorkflowPath {
        read_path,
        source_name,
    })
}

fn discover_candidates(dir: &Path) -> Result<(Vec<PathBuf>, bool), String> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(e) => {
            return Err(format!(
                "{}: could not read workflows directory: {e}\n  fix: check permissions, or pass -W <path> directly",
                safe_path(dir),
                e = safe_error(&e)
            ));
        }
    };
    let mut out = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|e| {
            format!(
                "{}: could not read a directory entry: {e}\n  fix: check permissions, or pass -W <path> directly",
                safe_path(dir),
                e = safe_error(&e)
            )
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("yml") || e.eq_ignore_ascii_case("yaml"))
            .unwrap_or(false);
        if is_yaml {
            if out.len() == MAX_REPORTED_CANDIDATES {
                return Ok((out, true));
            }
            out.push(path);
        }
    }
    Ok((out, false))
}
