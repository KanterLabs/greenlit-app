//! Resolves which workflow file `litci plan` should read when `-W`/
//! `--workflow` is omitted: exactly one `*.yml`/`*.yaml` file directly under
//! `.github/workflows/`, matching GitHub's own workflow-file location
//! convention. Ambiguous or absent discovery is reported with the fix of
//! passing `-W` explicitly -- never guessed.

use std::path::{Path, PathBuf};

const WORKFLOWS_DIR: &str = ".github/workflows";

/// Resolves the workflow file to plan: `explicit` if given, else the sole
/// `*.yml`/`*.yaml` file under [`WORKFLOWS_DIR`] relative to `cwd`.
pub(crate) fn resolve_workflow_path(
    explicit: Option<&Path>,
    cwd: &Path,
) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let dir = cwd.join(WORKFLOWS_DIR);
    let mut candidates = discover_candidates(&dir)?;
    candidates.sort();
    match candidates.len() {
        0 => Err(format!(
            "no workflow file found under {}\n  fix: pass -W <path> to select a workflow file, or add one under {WORKFLOWS_DIR}",
            dir.display()
        )),
        1 => Ok(candidates.remove(0)),
        _ => {
            let list = candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "multiple workflow files found under {}: {list}\n  fix: pass -W <path> to select one explicitly",
                dir.display()
            ))
        }
    }
}

fn discover_candidates(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "{}: could not read workflows directory: {e}\n  fix: check permissions, or pass -W <path> directly",
                dir.display()
            ));
        }
    };
    let mut out = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|e| {
            format!(
                "{}: could not read a directory entry: {e}\n  fix: check permissions, or pass -W <path> directly",
                dir.display()
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
            out.push(path);
        }
    }
    Ok(out)
}
