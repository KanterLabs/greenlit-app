//! Overlay-isolation wiring: the [`ContainerSpec`] that stacks a throwaway
//! writable overlay over a read-only repo bind.
//!
//! `PHASE-2-execution.md` ("Overlay isolation"): the repo is bound **read-only**
//! at the Docker level (defense in depth), and `greenlit-init` — the container
//! entrypoint — mounts an overlay whose lower layer is that read-only bind and
//! whose upper layer is a container-local tmpdir, exposing the merged writable
//! view at the workspace path. If unprivileged overlayfs is unavailable the
//! helper falls back to copy-in and logs it. The writable upper is never a host
//! bind, so the workflow container gets no host write access; `--write-back`
//! lifts the upper out through the Docker API instead (see [`crate::writeback`]).
//!
//! This module builds the container spec; per-step execution (env layering,
//! exec loop, output finalisation) is the execution task group's concern and
//! consumes the spec produced here.

use crate::engine::{BindMount, ContainerSpec};
use crate::image::INIT_IN_IMAGE_PATH;

/// In-container mount point of the read-only repository bind — the overlay
/// lower layer.
pub const CONTAINER_LOWER: &str = "/greenlit/lower";

/// In-container base directory for the overlay's writable side. The helper
/// creates `upper/` and `work/` beneath it; both are container-local, never a
/// host bind.
pub const CONTAINER_UPPER_BASE: &str = "/greenlit/upper";

/// In-container path of the overlay upper layer itself (`<base>/upper`), the
/// exact subtree `--write-back` exports as the change set.
pub const OVERLAY_UPPER_EXPORT_PATH: &str = "/greenlit/upper/upper";

/// Which isolation mechanism the entrypoint should use — the runtime-side
/// mirror of `greenlit-init`'s `--strategy`, kept independent so this crate does
/// not link the embedded helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationStrategy {
    /// Try overlay; fall back to copy-in when unprivileged overlayfs is
    /// unavailable, logging the fallback. The production default.
    #[default]
    Auto,
    /// Require overlay; the run fails if it cannot be mounted. Used to prove the
    /// overlay path actually exercised an overlay.
    Overlay,
    /// Skip overlay and copy the checkout in. Exercises the fallback path
    /// everywhere, including CI without overlayfs.
    CopyIn,
}

impl IsolationStrategy {
    /// The `--strategy` flag value the helper parses.
    #[must_use]
    pub fn as_flag(self) -> &'static str {
        match self {
            IsolationStrategy::Auto => "auto",
            IsolationStrategy::Overlay => "overlay",
            IsolationStrategy::CopyIn => "copy-in",
        }
    }
}

/// Build the isolated job container spec.
///
/// The returned spec binds `repo_host_path` **read-only** at
/// [`CONTAINER_LOWER`] and runs the helper as the entrypoint, which establishes
/// isolation and then execs `idle_command` (a long-lived process such as
/// `sleep infinity`) so the container stays alive for one exec per step. The
/// caller layers env, labels, network, and working directory onto the spec.
#[must_use]
pub fn isolation_container_spec(
    image: String,
    repo_host_path: &str,
    workspace: &str,
    strategy: IsolationStrategy,
    idle_command: Vec<String>,
) -> ContainerSpec {
    let entrypoint = vec![
        INIT_IN_IMAGE_PATH.to_string(),
        "--lower".to_string(),
        CONTAINER_LOWER.to_string(),
        "--upper".to_string(),
        CONTAINER_UPPER_BASE.to_string(),
        "--workspace".to_string(),
        workspace.to_string(),
        "--strategy".to_string(),
        strategy.as_flag().to_string(),
        // End of helper flags; everything after is the (idle) command verbatim.
        "--".to_string(),
    ];
    ContainerSpec {
        image,
        entrypoint,
        cmd: idle_command,
        binds: vec![BindMount {
            host_path: repo_host_path.to_string(),
            container_path: CONTAINER_LOWER.to_string(),
            read_only: true,
        }],
        ..ContainerSpec::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_flags_match_the_helper_vocabulary() {
        assert_eq!(IsolationStrategy::Auto.as_flag(), "auto");
        assert_eq!(IsolationStrategy::Overlay.as_flag(), "overlay");
        assert_eq!(IsolationStrategy::CopyIn.as_flag(), "copy-in");
    }

    #[test]
    fn spec_binds_repo_read_only_and_runs_helper_entrypoint() {
        let spec = isolation_container_spec(
            "greenlit/base:ubuntu-24.04-abc".to_string(),
            "/home/user/repo",
            "/home/runner/work/repo/repo",
            IsolationStrategy::Auto,
            vec!["sleep".to_string(), "infinity".to_string()],
        );

        // Exactly one bind: the repo, read-only, at the lower layer.
        assert_eq!(
            spec.binds,
            vec![BindMount {
                host_path: "/home/user/repo".to_string(),
                container_path: CONTAINER_LOWER.to_string(),
                read_only: true,
            }]
        );

        // The helper is the entrypoint, and the flag list ends with `--`.
        assert_eq!(
            spec.entrypoint.first().map(String::as_str),
            Some(INIT_IN_IMAGE_PATH)
        );
        assert_eq!(spec.entrypoint.last().map(String::as_str), Some("--"));
        assert!(
            spec.entrypoint
                .contains(&"/home/runner/work/repo/repo".to_string())
        );
        assert!(spec.entrypoint.contains(&"auto".to_string()));

        // The idle command follows the entrypoint verbatim.
        assert_eq!(spec.cmd, vec!["sleep".to_string(), "infinity".to_string()]);
    }

    #[test]
    fn strategy_selection_is_reflected_in_the_entrypoint() {
        let spec = isolation_container_spec(
            "img".to_string(),
            "/repo",
            "/ws",
            IsolationStrategy::CopyIn,
            vec!["true".to_string()],
        );
        assert!(spec.entrypoint.contains(&"copy-in".to_string()));
    }
}
