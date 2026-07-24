//! The request and response value types [`crate::ContainerEngine`] speaks in.
//!
//! Split from the trait itself so the *shape of what Greenlit asks a container
//! engine for* is reviewable on its own — the same reasoning that keeps
//! `greenlit-app`'s clap argument definitions in their own module. The trait
//! in the parent module stays a list of operations; everything here is data.
//!
//! Nothing in this module is Docker-specific. Field semantics are described in
//! terms of what Greenlit needs, and the backend maps them onto whatever its
//! API calls that concept.

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
/// expects. The base image build assembles this context; the engine only ships
/// it to the daemon.
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
///
/// A `host_path` that is not absolute names a **managed volume** rather than a
/// host directory — see [`ContainerSpec::binds`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindMount {
    /// Absolute host path to bind, or a managed volume name.
    pub host_path: String,
    /// Absolute path the bind appears at inside the container.
    pub container_path: String,
    /// Whether the bind is read-only. Greenlit sets this `true` for the repo
    /// lower layer; the writable overlay lives container-local, not on a host
    /// bind.
    pub read_only: bool,
}

/// One published container port.
///
/// `PHASE-4-environment.md` ("Service containers"): a service's `ports:` must
/// become a real publish so the job can reach it, where before Phase 4 they
/// were parsed and then silently dropped. Publishing is always bound to the
/// Greenlit bridge, never to every host interface — `host_ip` carries that
/// restriction explicitly rather than leaving it to a backend default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortBinding {
    /// The port inside the container.
    pub container_port: u16,
    /// `tcp` or `udp`.
    pub protocol: String,
    /// The port to publish on, or `None` to let the engine choose.
    pub host_port: Option<u16>,
    /// The address to publish on. Greenlit always sets this.
    pub host_ip: Option<String>,
}

/// A container health probe, as `--health-*` options request.
///
/// Durations are nanoseconds because that is the unit Docker's API models
/// them in; the caller parses GitHub's `--health-interval 10s` spellings and
/// converts once, rather than every backend re-parsing duration text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HealthCheck {
    /// The probe command. The first element is the Docker test *kind*
    /// (`CMD` or `CMD-SHELL`) and the rest are its arguments.
    pub test: Vec<String>,
    /// `--health-interval`.
    pub interval_nanos: Option<i64>,
    /// `--health-timeout`.
    pub timeout_nanos: Option<i64>,
    /// `--health-retries`.
    pub retries: Option<i64>,
    /// `--health-start-period`.
    pub start_period_nanos: Option<i64>,
}

/// A container to create.
///
/// The containment-breaking `options` rejection (privileged, host networking,
/// host PID/IPC, arbitrary host binds, …) is enforced by the execution task
/// group that builds these specs, before they reach the engine — the engine
/// faithfully creates what it is given.
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
    /// The network to attach to.
    ///
    /// This is a network *mode*, not only a name: besides a user-defined
    /// network's name it also accepts `container:<id>`, which joins an
    /// existing container's network namespace. Greenlit uses that second form
    /// for the netguard sidecar, which applies the run's filter rules inside
    /// the workflow container's own namespace (`PHASE-4-environment.md`
    /// "Network policy"). A workflow can never reach this field — `--network`
    /// in job-container `options:` is rejected outright.
    pub network: Option<String>,
    /// Additional DNS names this container answers to on its network.
    ///
    /// A service is reachable at its service id, which is what
    /// `PHASE-4-environment.md` means by "hostname = service key".
    pub network_aliases: Vec<String>,
    /// The container's own hostname.
    pub hostname: Option<String>,
    /// Linux capabilities to add.
    ///
    /// Only ever populated for Greenlit's own netguard sidecar, which needs
    /// `NET_ADMIN` to install the run's filter rules. A workflow container is
    /// never granted a capability — that is what makes the rules it runs
    /// under unremovable by anything inside it.
    pub cap_add: Vec<String>,
    /// Ports to publish.
    pub ports: Vec<PortBinding>,
    /// A health probe to attach, if the caller asked for one.
    pub healthcheck: Option<HealthCheck>,
    /// Labels to stamp on the container (Greenlit ownership tags).
    pub labels: Vec<(String, String)>,
    /// Binds for the container.
    ///
    /// An absolute `host_path` is a real host directory; Greenlit populates
    /// that only with the read-only repository lower layer for overlay
    /// isolation. A relative `host_path` names a managed volume instead, which
    /// is how the run-scoped workspace volume reaches a Docker-action sibling.
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

/// The terminal result of an [`crate::ContainerEngine::exec`] — its exit code.
///
/// Streamed stdout/stderr are delivered incrementally through the
/// [`super::ExecOutputSink`] while the command runs; only the exit code remains
/// at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOutput {
    /// Process exit code (`0` on success).
    pub exit_code: i64,
}

/// A container's health, as its probe last reported it.
///
/// Mirrors Docker's own states one-for-one so the health gate can distinguish
/// "no probe was configured" from "the probe has not passed yet" — the two
/// cases need opposite handling, since a service with no probe is ready as
/// soon as it starts while one that is still `Starting` is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// The image and spec configured no probe.
    None,
    /// A probe is configured and still inside its start period / retries.
    Starting,
    /// The probe has passed.
    Healthy,
    /// The probe has failed its configured number of retries.
    Unhealthy,
}

/// The parts of a container's inspected state Greenlit acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerState {
    /// Whether the container's main process is running.
    pub running: bool,
    /// Its exit code, once it has stopped.
    pub exit_code: Option<i64>,
    /// What its health probe reports.
    pub health: HealthState,
}

/// One image, as [`crate::ContainerEngine::list_images`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSummary {
    /// The image id.
    pub id: String,
    /// Every `name:tag` pointing at it.
    pub tags: Vec<String>,
    /// On-disk size in bytes, as the engine reports it.
    pub size_bytes: u64,
}

/// What Greenlit needs to know about a network it created.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetworkInfo {
    /// The bridge's own IPv4 address on the host, which is the address a
    /// container attached to it reaches the host at.
    ///
    /// `PHASE-4-environment.md` ("Network policy"): "Bind only on the
    /// Greenlit bridge gateway". The shim binds here rather than on every
    /// host interface, so nothing outside the run can reach it, and the
    /// container addresses it here rather than at a loopback it does not
    /// share.
    pub gateway: Option<String>,
}
