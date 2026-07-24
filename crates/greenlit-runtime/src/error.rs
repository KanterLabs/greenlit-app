//! Runtime errors.
//!
//! The UX invariant in `AGENTS.md` ("Global invariants") forbids surfacing a
//! raw connection error: "No raw stack traces, no 'cannot connect to Docker
//! daemon'." Engine-*detection* outcomes never travel as errors at all — they
//! are the [`crate::detect::EngineState`] enum, each failing variant carrying a
//! message plus the one action that fixes it. This type is for failures *after*
//! a daemon has been reached (an image pull that 404s, an exec that the daemon
//! rejects), where the underlying cause is real and worth propagating with
//! context rather than a hand-written fix.

use std::fmt;

/// A container-engine operation failed after a daemon was reached.
///
/// Detection-time conditions (no daemon, daemon stopped, engine absent) are not
/// represented here; they are [`crate::detect::EngineState`] variants so every
/// one carries its own fix action.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeError {
    /// Connecting a [`bollard`] client to a resolved endpoint failed before any
    /// request was issued (bad `DOCKER_HOST` URL, unreadable socket path).
    #[error("could not open a Docker API connection to {endpoint}: {source}")]
    Connect {
        /// The endpoint that was being connected to, for the operator message.
        endpoint: String,
        /// The underlying bollard connection error.
        #[source]
        source: bollard::errors::Error,
    },

    /// `DOCKER_HOST` named a transport v0's engine backend does not support
    /// (e.g. `ssh://`). Carries the one action that resolves it, per the UX
    /// invariant.
    #[error("unsupported DOCKER_HOST transport in `{value}`: {fix}")]
    UnsupportedDockerHost {
        /// The offending `DOCKER_HOST` value.
        value: String,
        /// The one action that resolves it.
        fix: String,
    },

    /// `DOCKER_HOST` named a `tcp://`/`http(s)://` endpoint whose host is not
    /// `localhost` or a loopback address.
    ///
    /// v0 targets a single local Linux x86_64 daemon (`greenlit-v0-spec.md`
    /// "Tech"): the repository checkout is bind-mounted by *host path*, and a
    /// Docker bind-mount path is resolved by the daemon it is sent to
    /// (<https://docs.docker.com/engine/storage/bind-mounts/>). Against a
    /// remote daemon that path would resolve on the remote machine instead —
    /// silently binding the wrong directory, or an unrelated one the daemon
    /// happens to create. Carries the one action that resolves it.
    #[error("DOCKER_HOST points at a remote daemon (`{value}`): {fix}")]
    RemoteDockerHost {
        /// The offending `DOCKER_HOST` value.
        value: String,
        /// The one action that resolves it.
        fix: String,
    },

    /// A Docker Engine API call returned an error (pull, build, commit,
    /// create/start/stop/remove container, exec, network op).
    #[error("Docker API request failed during {operation}: {source}")]
    Api {
        /// Which trait operation was in flight, for the operator message.
        operation: Operation,
        /// The underlying bollard API error.
        #[source]
        source: bollard::errors::Error,
    },
}

impl RuntimeError {
    /// Wraps a bollard error raised while performing `operation`.
    pub(crate) fn api(operation: Operation, source: bollard::errors::Error) -> Self {
        RuntimeError::Api { operation, source }
    }
}

/// The [`crate::ContainerEngine`] operation that produced a [`RuntimeError`].
///
/// Kept as a small enum rather than a free string so the error surface stays
/// closed and greppable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// Pulling an image.
    PullImage,
    /// Inspecting an image to test for its presence.
    InspectImage,
    /// Building an image from a context tar.
    BuildImage,
    /// Committing a container to an image.
    CommitContainer,
    /// Creating a container.
    CreateContainer,
    /// Starting a container.
    StartContainer,
    /// Stopping a container.
    StopContainer,
    /// Removing a container.
    RemoveContainer,
    /// Exporting a filesystem subtree from a container as a tar archive.
    ExportPath,
    /// Streaming a container's own stdout/stderr log.
    ContainerLogs,
    /// Creating an exec instance.
    CreateExec,
    /// Starting an exec instance / draining its output.
    StartExec,
    /// Inspecting an exec instance for its exit code.
    InspectExec,
    /// Creating a network.
    CreateNetwork,
    /// Removing a network.
    RemoveNetwork,
    /// Inspecting a container for its state, including health.
    InspectContainer,
    /// Creating a named volume.
    CreateVolume,
    /// Removing a named volume.
    RemoveVolume,
    /// Listing images, filtered by Greenlit's ownership label.
    ListImages,
    /// Removing an image.
    RemoveImage,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Operation::PullImage => "pull image",
            Operation::InspectImage => "inspect image",
            Operation::BuildImage => "build image",
            Operation::CommitContainer => "commit container",
            Operation::CreateContainer => "create container",
            Operation::StartContainer => "start container",
            Operation::StopContainer => "stop container",
            Operation::RemoveContainer => "remove container",
            Operation::ExportPath => "export path",
            Operation::ContainerLogs => "stream container logs",
            Operation::CreateExec => "create exec",
            Operation::StartExec => "start exec",
            Operation::InspectExec => "inspect exec",
            Operation::CreateNetwork => "create network",
            Operation::RemoveNetwork => "remove network",
            Operation::InspectContainer => "inspect container",
            Operation::CreateVolume => "create volume",
            Operation::RemoveVolume => "remove volume",
            Operation::ListImages => "list images",
            Operation::RemoveImage => "remove image",
        };
        f.write_str(name)
    }
}
