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

use crate::engine::{CommitSpec, ContainerEngine, ExecSpec, SinkNull};
use crate::executor::ExecError;
use std::path::Path;

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

/// The image repository converged per-repo images live in.
const CONVERGED_REPO: &str = "greenlit/converged";

/// The tag a converged image for this checkout and runner label gets.
///
/// `PHASE-4-environment.md`: "commit only the installed-tools layer as a
/// per-repo image (`greenlit/<repo-hash>:<runner-label>`); subsequent runs
/// start from it."
///
/// The hash covers the *base image tag* as well as the checkout path. The
/// base tag is already content-addressed over the Dockerfile and the embedded
/// init helper, so folding it in means a base rebuild automatically retires
/// every converged image built on the old one — rather than leaving a stale
/// layer sitting on a base that no longer exists.
pub(crate) fn converged_tag(repo_host_path: &Path, base_tag: &str, label: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in repo_host_path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .chain(b"\0")
        .chain(base_tag.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{CONVERGED_REPO}-{hash:016x}:{label}")
}

/// Which commands a finished job actually provisioned.
///
/// Read from the markers the shims leave, so it reflects what was installed
/// rather than what might have been.
pub(crate) async fn provisioned_commands(
    engine: &dyn ContainerEngine,
    container: &str,
) -> Vec<String> {
    let spec = ExecSpec {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            "ls -1 /greenlit/provisioned 2>/dev/null || true".to_string(),
        ],
        env: Vec::new(),
        working_dir: None,
    };
    let mut sink = crate::executor::context::CaptureSink::default();
    if engine.exec(container, &spec, &mut sink).await.is_err() {
        return Vec::new();
    }
    sink.text()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Builds this repository's converged image from a **clean** base container.
///
/// `PHASE-4-environment.md` says to commit "only the installed-tools layer".
/// Committing the finished job container instead would bake in everything the
/// workflow wrote — its workspace, its `/tmp`, its outputs — and the next run
/// would start from a filesystem carrying the previous run's leftovers. That
/// is not a theoretical concern: it was caught by a probe whose second run
/// failed because a file the first run appended to was still there.
///
/// So convergence replays the installs in a fresh container started from the
/// base image, which never executes a single line of workflow code, and
/// commits *that*. The cost is installing once more at teardown; the benefit
/// is that a converged image contains tools and nothing else.
///
/// Best effort throughout: a convergence that fails costs the next run some
/// seconds, and turning that into a failed workflow would be an absurd trade.
pub(crate) async fn build_converged(
    engine: &dyn ContainerEngine,
    base_image: &str,
    manifest: &manifest::Manifest,
    commands: &[String],
    tag: &str,
) {
    let installs: Vec<String> = commands
        .iter()
        .filter_map(|command| manifest.recipe(command).map(shim::install_command))
        .collect();
    if installs.is_empty() {
        return;
    }
    let Some((repo, version)) = tag.rsplit_once(':') else {
        return;
    };

    let spec = crate::engine::ContainerSpec {
        image: base_image.to_string(),
        entrypoint: vec!["/bin/sh".to_string(), "-c".to_string()],
        cmd: vec![format!("set -e\n{}", installs.join("\n"))],
        labels: vec![("greenlit.managed".to_string(), "1".to_string())],
        ..crate::engine::ContainerSpec::default()
    };

    let Ok(builder) = engine.create_container(&spec).await else {
        return;
    };
    let ran = engine.run_container(&builder, &mut SinkNull).await;
    if matches!(ran, Ok(output) if output.exit_code == 0) {
        let commit = CommitSpec {
            container: builder.clone(),
            repo: repo.to_string(),
            tag: version.to_string(),
        };
        if engine.commit_container(&commit).await.is_err() {
            tracing::debug!(
                target: "greenlit_runtime::provision",
                tag,
                "could not commit the converged image; the next run reinstalls instead"
            );
        }
    }
    let _ = engine.remove_container(&builder).await;
}

#[cfg(test)]
mod tests {
    use super::mentioned_commands;

    #[test]
    fn a_converged_tag_is_stable_per_checkout_and_label() {
        use super::converged_tag;
        use std::path::Path;

        let repo = Path::new("/home/user/project");
        let base = "greenlit/base:ubuntu-24.04-abcdef0123456789";
        assert_eq!(
            converged_tag(repo, base, "ubuntu-24.04"),
            converged_tag(repo, base, "ubuntu-24.04"),
            "the same checkout must reuse its own image"
        );
        // Different label, different image: the whole point is that 24.04 and
        // 22.04 never share provisioned tools.
        assert_ne!(
            converged_tag(repo, base, "ubuntu-24.04"),
            converged_tag(repo, base, "ubuntu-22.04")
        );
        // Different checkout, different image.
        assert_ne!(
            converged_tag(repo, base, "ubuntu-24.04"),
            converged_tag(Path::new("/home/user/other"), base, "ubuntu-24.04")
        );
    }

    #[test]
    fn a_rebuilt_base_retires_the_converged_image_built_on_it() {
        use super::converged_tag;
        use std::path::Path;

        let repo = Path::new("/home/user/project");
        assert_ne!(
            converged_tag(repo, "greenlit/base:ubuntu-24.04-aaaa", "ubuntu-24.04"),
            converged_tag(repo, "greenlit/base:ubuntu-24.04-bbbb", "ubuntu-24.04"),
            "a stale layer on a base that no longer exists would be worse than reinstalling"
        );
    }

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
