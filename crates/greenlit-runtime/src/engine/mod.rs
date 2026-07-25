//! The [`ContainerEngine`] port: the single trait every container operation
//! flows through.
//!
//! `greenlit-v0-spec.md` ("Tech"): "All engine access goes through one Rust
//! trait (Docker-API client behind it), so future platforms and architectures
//! are ports, not rewrites." The Docker/bollard backend lives in
//! [`crate::docker`]; tests drive a fake implementation of this same trait.
//!
//! The trait is object-safe (via `#[async_trait]`) so the engine can be held as
//! `Box<dyn ContainerEngine>` behind the port boundary. Every method is async.
//!
//! The request and response *data* types live in the `spec` submodule and are re-exported
//! here, so this module reads as the list of operations Greenlit performs.

mod spec;

use async_trait::async_trait;

use crate::error::RuntimeError;
use crate::progress::ProgressSink;

pub use spec::{
    BindMount, BuildSpec, CommitSpec, ContainerSpec, ContainerState, ExecOutput, ExecSpec,
    HealthCheck, HealthState, ImageIdentity, ImageSummary, NetworkInfo, PortBinding, RegistryAuth,
};

/// Receives an exec's stdout/stderr as the daemon streams it.
///
/// The execution task group implements this to fold `::group::` blocks, apply
/// `::add-mask::` redaction, and parse workflow command files — all of which
/// must happen on the live stream, chunk by chunk, not on a buffered whole.
/// Chunk boundaries are wherever the daemon framed them and carry no semantic
/// meaning (a line may span two chunks).
pub trait ExecOutputSink: Send {
    /// A chunk of standard output arrived.
    fn on_stdout(&mut self, chunk: &[u8]);
    /// A chunk of standard error arrived.
    fn on_stderr(&mut self, chunk: &[u8]);
}

/// A sink that discards all output — for callers that only need the exit code.
#[derive(Debug, Default, Clone, Copy)]
pub struct SinkNull;

impl ExecOutputSink for SinkNull {
    fn on_stdout(&mut self, _chunk: &[u8]) {}
    fn on_stderr(&mut self, _chunk: &[u8]) {}
}

/// The container-engine port.
///
/// One trait, every backend behind it. Methods map one-to-one onto the Docker
/// Engine API operations Greenlit needs. Implementations must never shell out
/// to the `docker` binary (`AGENTS.md`).
#[async_trait]
pub trait ContainerEngine: Send + Sync {
    /// Pull an image by `name:tag` reference so it is present locally,
    /// reporting layer progress to `progress` as the daemon streams it.
    ///
    /// `auth`, when supplied, authenticates the pull against a private
    /// registry with already-host-resolved credentials
    /// (`jobs.<id>.container.credentials`, `PHASE-3-actions.md`) — `None`
    /// pulls anonymously/using the daemon's own configured credential store,
    /// exactly like every pull before this parameter existed.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the daemon rejects or fails the pull.
    async fn pull_image(
        &self,
        image: &str,
        auth: Option<&RegistryAuth>,
        progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError>;

    /// Whether an image with the given `name:tag` reference already exists
    /// locally.
    ///
    /// Used to build the convergent base image only on first use
    /// (`PHASE-2-execution.md` "Base image and private init helper": "Build
    /// through the engine API on first use"); a present content-hash-tagged
    /// image is reused rather than rebuilt.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] only for a real inspection failure; a
    /// simple "image not found" is reported as `Ok(false)`, not an error.
    async fn image_exists(&self, image: &str) -> Result<bool, RuntimeError>;

    /// Returns the immutable identity and platform of a materialized image.
    ///
    /// Backends without an inspect equivalent return `None`; callers that
    /// require a lock must fail closed rather than inventing an identity.
    async fn image_identity(&self, _image: &str) -> Result<Option<ImageIdentity>, RuntimeError> {
        Ok(None)
    }

    /// Build an image from a context tar, tagging it `spec.tag`, reporting
    /// daemon build-output lines to `progress`.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the build fails.
    async fn build_image(
        &self,
        spec: &BuildSpec,
        progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError>;

    /// Commit a container into a new image, returning the new image id.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the commit fails.
    async fn commit_container(&self, spec: &CommitSpec) -> Result<String, RuntimeError>;

    /// Every image carrying `label`, given as `key=value`.
    ///
    /// `litci clean` uses this to find Greenlit's own converged images by the
    /// ownership label it stamps, rather than by pattern-matching tag text
    /// that a user's own image could imitate.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the listing fails.
    async fn list_images(&self, label: &str) -> Result<Vec<ImageSummary>, RuntimeError>;

    /// Remove an image by reference or id.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if removal fails. An image that is
    /// already gone is not an error.
    async fn remove_image(&self, image: &str) -> Result<(), RuntimeError>;

    /// Create a container from `spec`, returning its id.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if creation fails.
    async fn create_container(&self, spec: &ContainerSpec) -> Result<String, RuntimeError>;

    /// Start a previously created container.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the start fails.
    async fn start_container(&self, id: &str) -> Result<(), RuntimeError>;

    /// Stop a running container.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the stop fails.
    async fn stop_container(&self, id: &str) -> Result<(), RuntimeError>;

    /// Remove a container (force-removing if running).
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if removal fails.
    async fn remove_container(&self, id: &str) -> Result<(), RuntimeError>;

    /// The parts of a container's state Greenlit acts on, including what its
    /// health probe last reported.
    ///
    /// The service health gate polls this until a service reports
    /// [`HealthState::Healthy`] or its deadline elapses
    /// (`PHASE-4-environment.md`: "health-check gating … poll until healthy or
    /// timeout").
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the inspection fails.
    async fn inspect_container(&self, id: &str) -> Result<ContainerState, RuntimeError>;

    /// Create a named volume, if it does not already exist.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if creation fails.
    async fn create_volume(&self, name: &str) -> Result<(), RuntimeError>;

    /// Remove a named volume.
    ///
    /// Greenlit's run-scoped volumes are removed at the end of the run that
    /// created them; before this existed they accumulated on the host until an
    /// operator pruned them by hand.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if removal fails. A volume that is
    /// already gone is not an error.
    async fn remove_volume(&self, name: &str) -> Result<(), RuntimeError>;

    /// Run `spec` as an exec inside container `container`, streaming stdout and
    /// stderr to `sink` as they arrive and returning the exit code.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the daemon rejects the exec, the output
    /// stream errors, or the exit code cannot be read.
    async fn exec(
        &self,
        container: &str,
        spec: &ExecSpec,
        sink: &mut (dyn ExecOutputSink + Send),
    ) -> Result<ExecOutput, RuntimeError>;

    /// Runs a *created* container's own entrypoint/cmd to completion,
    /// streaming its stdout/stderr to `sink` from start to exit and
    /// returning its exit code.
    ///
    /// Unlike [`Self::exec`] (which runs an extra command inside an
    /// already-idling container), this drives the container's own primary
    /// process — the shape a Docker action's sibling container needs
    /// (`crate::executor::actions::docker_action`): the sibling is created
    /// with the action's real entrypoint/args as its `cmd`, started, and run
    /// to completion here, exactly like `docker run` (as opposed to
    /// `docker exec` into a long-lived idle container).
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if starting the container, streaming its
    /// logs, or waiting for it to exit fails at the daemon level.
    async fn run_container(
        &self,
        id: &str,
        sink: &mut (dyn ExecOutputSink + Send),
    ) -> Result<ExecOutput, RuntimeError>;

    /// Best-effort termination of a still-running exec whose process wrote its
    /// own PID to `pid_file` at start (see `crate::executor::step`'s
    /// timeout-wrapper).
    ///
    /// Docker's Engine API has no "kill this exec" endpoint — an exec's
    /// process lives on independently of the container once started, so
    /// dropping an awaited `exec` future (e.g. via `tokio::time::timeout`)
    /// only stops *streaming* it, not the process itself, letting it keep
    /// running and race a later step
    /// (<https://github.com/moby/moby/issues/9098>). The reliable workaround —
    /// implemented here once, for every backend, in terms of [`Self::exec`] —
    /// is to signal the process from a *fresh exec into the same container*:
    /// that new exec joins the container's own pid namespace, so the PID the
    /// timed-out process observed about itself (its own `$$`, from inside
    /// that same namespace) is meaningful there. This process (an ordinary,
    /// non-root CLI) generally cannot signal the daemon's containerized
    /// processes directly: the `Pid` Docker's exec-inspect API reports is
    /// numbered in the *host's* pid namespace, and belongs to the daemon, not
    /// the invoking user.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] only if the termination exec itself could
    /// not be dispatched; a process that already exited is not an error.
    async fn terminate(&self, container: &str, pid_file: &str) -> Result<(), RuntimeError> {
        // Escalates SIGTERM to SIGKILL after a short grace period, targeting
        // the process group first (falling back to the bare pid) so a still-
        // running child of the step's shell is caught too. Every step of the
        // pipeline tolerates the pid already being gone.
        let script = format!(
            "pid=$(cat {pid_file} 2>/dev/null) || exit 0; \
             [ -n \"$pid\" ] || exit 0; \
             kill -TERM -- -\"$pid\" 2>/dev/null || kill -TERM \"$pid\" 2>/dev/null || true; \
             sleep 1; \
             kill -KILL -- -\"$pid\" 2>/dev/null || kill -KILL \"$pid\" 2>/dev/null || true"
        );
        let spec = ExecSpec {
            cmd: vec!["sh".to_string(), "-c".to_string(), script],
            env: Vec::new(),
            working_dir: None,
        };
        self.exec(container, &spec, &mut SinkNull).await?;
        Ok(())
    }

    /// Export the filesystem subtree at `path` inside `container` as an
    /// uncompressed tar archive.
    ///
    /// Greenlit uses this to lift the overlay upper layer out of a finished
    /// container for `--write-back` (`PHASE-2-execution.md`: "export the
    /// upper-layer diff") — the container never gets host write access, so the
    /// diff leaves through the Docker API rather than a host bind.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the daemon rejects the request or the
    /// archive stream errors.
    async fn export_path(&self, container: &str, path: &str) -> Result<Vec<u8>, RuntimeError>;

    /// Create a user-defined bridge network, returning its id.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if creation fails.
    async fn create_network(&self, name: &str) -> Result<String, RuntimeError>;

    /// The gateway address of a network Greenlit created.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if the inspection fails.
    async fn inspect_network(&self, name: &str) -> Result<NetworkInfo, RuntimeError>;

    /// Remove a network by name or id.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if removal fails.
    async fn remove_network(&self, name: &str) -> Result<(), RuntimeError>;
}
