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

use async_trait::async_trait;

use crate::error::RuntimeError;
use crate::progress::ProgressSink;

/// Private-registry credentials for an image pull.
///
/// `PHASE-3-actions.md` ("Job-container private-registry credentials"):
/// `jobs.<id>.container.credentials.{username,password}` are resolved
/// host-side (against the `secrets` context, like any other `env:`/`with:`
/// value) *before* reaching the engine — this type is the already-resolved
/// pair, never a `${{ }}` expression. Never logged or included in any
/// `Debug`/error text a step's own output could echo back; callers mask both
/// fields with the run's `greenlit_engine::execution::Masker` the same way
/// every other resolved secret is (`AGENTS.md`: "secret values are masked in
/// all log output").
#[derive(Clone, PartialEq, Eq)]
pub struct RegistryAuth {
    /// The registry username.
    pub username: String,
    /// The registry password (or token).
    pub password: String,
}

impl std::fmt::Debug for RegistryAuth {
    /// Deliberately redacted: a `Debug`-formatted `RegistryAuth` must never
    /// leak the password into a log line, panic message, or test failure
    /// output that a masker never gets a chance to see.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryAuth")
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

/// A container image build request.
///
/// The `context_tar` is an uncompressed (or gzip/xz) tar of the build context
/// with the `dockerfile` inside it — the exact bytes Docker's `/build` endpoint
/// expects. The base image build (a later Phase 2 task group) assembles this
/// context; the engine only ships it to the daemon.
#[derive(Debug, Clone)]
pub struct BuildSpec {
    /// Tar archive of the build context (must contain `dockerfile`).
    pub context_tar: Vec<u8>,
    /// Path of the Dockerfile within the context (usually `Dockerfile`).
    pub dockerfile: String,
    /// The `name:tag` to tag the built image with.
    pub tag: String,
    /// `ARG` values passed to the build.
    pub build_args: Vec<(String, String)>,
}

/// A request to commit a running/stopped container into a new image.
#[derive(Debug, Clone)]
pub struct CommitSpec {
    /// The container id (or name) to commit.
    pub container: String,
    /// The image repository to commit into (e.g. `greenlit/myrepo`).
    pub repo: String,
    /// The tag to apply (e.g. a content hash).
    pub tag: String,
}

/// A host-directory bind into the container.
///
/// Greenlit's only sanctioned host bind is the repository checkout, mounted
/// **read-only** as the overlay lower layer — defense in depth beneath the
/// container-local overlay isolation (`PHASE-2-execution.md` "Overlay
/// isolation": "The host repo bind mount is read-only at the Docker level,
/// independent of the overlay"). A read-write host bind is never constructed for
/// a workflow container; the `read_only` flag exists so the read-only intent is
/// explicit at the type level rather than implied by a string suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindMount {
    /// Absolute host path to bind.
    pub host_path: String,
    /// Absolute path the bind appears at inside the container.
    pub container_path: String,
    /// Whether the bind is read-only. Greenlit sets this `true` for the repo
    /// lower layer; the writable overlay lives container-local, not on a host
    /// bind.
    pub read_only: bool,
}

/// A container to create.
///
/// Only the fields Phase 2's shell execution and overlay isolation need are
/// modelled here. The containment-breaking `options` rejection (privileged, host
/// networking, host PID/IPC, arbitrary host binds, …) is enforced by the
/// execution task group that builds these specs, before they reach the engine —
/// the engine faithfully creates what it is given.
#[derive(Debug, Clone, Default)]
pub struct ContainerSpec {
    /// Image reference to run (`name:tag` or id).
    pub image: String,
    /// Optional explicit container name.
    pub name: Option<String>,
    /// Entrypoint override; empty means the image default.
    pub entrypoint: Vec<String>,
    /// Command / args; empty means the image default.
    pub cmd: Vec<String>,
    /// Environment variables as `(key, value)` pairs.
    pub env: Vec<(String, String)>,
    /// Working directory inside the container.
    pub working_dir: Option<String>,
    /// Name of a user-defined network to attach to, if any.
    pub network: Option<String>,
    /// Labels to stamp on the container (Greenlit ownership tags).
    pub labels: Vec<(String, String)>,
    /// Host binds for the container. Greenlit populates this only with the
    /// read-only repository lower layer for overlay isolation.
    pub binds: Vec<BindMount>,
}

/// A single `exec` inside an already-running container — one workflow step.
#[derive(Debug, Clone, Default)]
pub struct ExecSpec {
    /// The command to run (argv form, already shell-resolved by the caller).
    pub cmd: Vec<String>,
    /// Environment variables layered for this step, as `(key, value)`.
    pub env: Vec<(String, String)>,
    /// Working directory for this exec.
    pub working_dir: Option<String>,
}

/// The terminal result of an [`ContainerEngine::exec`] — its exit code.
///
/// Streamed stdout/stderr are delivered incrementally through the
/// [`ExecOutputSink`] while the command runs; only the exit code remains at the
/// end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOutput {
    /// Process exit code (`0` on success).
    pub exit_code: i64,
}

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
/// Engine API operations Phase 2 needs. Implementations must never shell out to
/// the `docker` binary (`AGENTS.md`).
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

    /// Remove a network by name or id.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Api`] if removal fails.
    async fn remove_network(&self, name: &str) -> Result<(), RuntimeError>;
}
