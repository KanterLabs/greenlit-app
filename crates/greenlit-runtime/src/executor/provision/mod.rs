//! Convergent images and command-level lazy provisioning.
//!
//! `PHASE-4-environment.md` ("Convergent images"). Greenlit's base image is
//! deliberately slim — bash, git, curl, wget, jq, tar, unzip,
//! build-essential — while a GitHub runner image carries hundreds of tools.
//! Shipping all of them would make the base image enormous for every user to
//! serve the few commands any one repository actually calls.
//!
//! Instead each repository *converges*: the tools its workflows actually use
//! are installed on demand, at the versions the matching runner image
//! carries, and the result is committed as a per-repo image so later runs
//! start from it.
//!
//! [`manifest`] is the pure half — which commands the runner image has, and
//! how each is installed. It has no I/O, so the rule is pinned by tables.

pub(crate) mod fetch;
pub(crate) mod manifest;
pub(crate) mod shim;

use crate::engine::{ContainerEngine, ExecSpec, SinkNull};
use crate::executor::ExecError;

/// Where provisioning shims land. **Last** on `PATH`, so the moment a real
/// tool exists the shim stops being reached at all.
pub(crate) const SHIM_DIR: &str = "/greenlit/shims";

/// Installs shims for every manifest-known command the image lacks.
///
/// Only commands the job's own scripts mention get a shim: generating one for
/// all sixty-odd manifest commands would write sixty files into every
/// container to serve the two a workflow actually calls.
///
/// # Errors
/// Returns [`ExecError`] if the shims cannot be written. A command that is
/// already present, or that the manifest does not describe, is skipped —
/// `PHASE-4-environment.md`: a command absent from the manifest "has no shim
/// and fails normally".
pub(crate) async fn install_shims(
    engine: &dyn ContainerEngine,
    container: &str,
    manifest: &manifest::Manifest,
    wanted: &[String],
    label: &str,
) -> Result<Vec<String>, ExecError> {
    let mut script = format!("mkdir -p {SHIM_DIR}\n");
    let mut installed = Vec::new();

    for command in wanted {
        let Some(recipe) = manifest.recipe(command) else {
            continue;
        };
        // `command -v` answers from the container's own PATH, which at this
        // point does not yet include the shim directory -- so this asks
        // "does the image already have it?", not "did we already shim it?".
        let body = shim::render(command, recipe, SHIM_DIR, label);
        script.push_str(&format!(
            "if ! command -v {command} >/dev/null 2>&1; then\n\
             cat > {SHIM_DIR}/{command} <<'GREENLIT_SHIM_{command}'\n\
             {body}GREENLIT_SHIM_{command}\n\
             chmod 0755 {SHIM_DIR}/{command}\n\
             fi\n"
        ));
        installed.push(command.clone());
    }

    if installed.is_empty() {
        return Ok(Vec::new());
    }

    let spec = ExecSpec {
        cmd: vec!["sh".to_string(), "-c".to_string(), script],
        env: Vec::new(),
        working_dir: None,
    };
    engine.exec(container, &spec, &mut SinkNull).await?;
    Ok(installed)
}

/// Command-looking tokens in a job's `run:` scripts.
///
/// Deliberately generous and deliberately cheap: this only decides which
/// shims to *offer*. A token that is not a manifest command is ignored, and a
/// command already in the image is skipped, so over-collecting costs nothing
/// while under-collecting costs a confusing failure mid-step.
pub(crate) fn mentioned_commands(
    scripts: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for script in scripts {
        for token in script
            .as_ref()
            .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '+'))
        {
            if token.is_empty() || found.iter().any(|seen| seen == token) {
                continue;
            }
            found.push(token.to_string());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::mentioned_commands;

    #[test]
    fn tokens_are_collected_once_each_in_order() {
        let found = mentioned_commands(["jq --version && jq -r .", "make build"]);
        assert_eq!(
            found.iter().filter(|token| *token == "jq").count(),
            1,
            "a command named twice needs one shim, not two"
        );
        assert!(found.contains(&"make".to_string()));
    }

    #[test]
    fn punctuation_never_becomes_part_of_a_command() {
        let found = mentioned_commands(["cat file.txt | jq '.a'"]);
        assert!(found.contains(&"jq".to_string()));
        assert!(!found.iter().any(|token| token.contains('|')));
        assert!(!found.iter().any(|token| token.contains('.')));
    }
}
