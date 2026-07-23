//! Three-state container-engine detection.
//!
//! `greenlit-v0-spec.md` ("Tech", "Zero prerequisites"): "engine detection is
//! three-state — reachable → run; installed but daemon stopped → offer to start
//! it (`sudo systemctl start docker`, honoring socket activation and rootless
//! `--user` daemons); absent → `litci setup` installs Docker via the official
//! script. Detection order: `DOCKER_HOST` → Docker socket → Podman socket.
//! Greenlit never surfaces 'cannot connect to Docker daemon' — every failure
//! maps to a state plus the one action that fixes it."
//!
//! The detection *algorithm* lives here and is pure over an injected
//! [`EngineProber`]; all real I/O (env reads, socket pings, `PATH` probing)
//! lives behind the prober so the three states are exercised in tests without a
//! Docker daemon (`PHASE-2-execution.md` exit criterion 4, and `TESTING.md`:
//! "the engine prober" is a true external boundary that is mocked).

use async_trait::async_trait;

use crate::engine::ContainerEngine;

/// A place a Docker Engine API daemon might be listening.
///
/// Candidates are tried in the spec's detection order. Podman ships a
/// Docker-API-compatible socket, so the same bollard client speaks to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// The `DOCKER_HOST` environment value (a `unix://`, `tcp://`, or `ssh://`
    /// URL), tried first when set.
    DockerHost(String),
    /// The default Docker Unix socket, `/var/run/docker.sock`.
    DockerSocket,
    /// A Podman Docker-compatible Unix socket at this filesystem path.
    PodmanSocket(String),
}

impl Endpoint {
    /// The default Docker socket path Greenlit probes.
    pub const DOCKER_SOCKET_PATH: &'static str = "/var/run/docker.sock";

    /// A short operator-facing description of the endpoint.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Endpoint::DockerHost(url) => format!("DOCKER_HOST ({url})"),
            Endpoint::DockerSocket => {
                format!("Docker socket ({})", Endpoint::DOCKER_SOCKET_PATH)
            }
            Endpoint::PodmanSocket(path) => format!("Podman socket ({path})"),
        }
    }
}

/// A message plus the single action that resolves a detection failure.
///
/// This is how the UX invariant ("every error maps to a state plus the one
/// action that fixes it") is upheld: a failing detection outcome is never a raw
/// connection error, always an [`EngineFix`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFix {
    /// What is wrong, phrased for the operator.
    pub message: String,
    /// The one command/action that resolves it.
    pub action: String,
}

/// The outcome of engine detection — the spec's three reachability states,
/// plus one hard-rejection state for a `DOCKER_HOST` value v0's engine
/// backend must refuse outright rather than probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    /// A daemon was reached at `endpoint`; the caller connects the engine to it.
    Available {
        /// The reachable endpoint, in detection-order priority.
        endpoint: Endpoint,
    },
    /// A Docker/Podman client binary is installed but no daemon answered — the
    /// daemon is stopped. Carries the start action.
    DaemonStopped(EngineFix),
    /// No engine is installed at all. Carries the `litci setup` action.
    NotInstalled(EngineFix),
    /// `DOCKER_HOST` names a transport or a non-local daemon v0's engine
    /// backend refuses outright (see `reject_docker_host`) — reported
    /// immediately rather than silently falling back to the local Docker/
    /// Podman socket, since that would ignore the operator's explicit
    /// `DOCKER_HOST` and run against the wrong daemon by surprise. Carries the
    /// one action that resolves it.
    UnsupportedDockerHost(EngineFix),
}

/// The injected boundary that performs detection's real I/O.
///
/// Every method here touches the outside world (environment, filesystem,
/// sockets, `PATH`). Keeping them behind a trait lets the pure [`detect`]
/// algorithm be tested across all three states with a fake.
#[async_trait]
pub trait EngineProber: Send + Sync {
    /// The `DOCKER_HOST` value, if the environment sets it.
    fn docker_host(&self) -> Option<String>;

    /// Candidate Podman socket paths in priority order — conventionally the
    /// rootful `/run/podman/podman.sock` then the rootless
    /// `$XDG_RUNTIME_DIR/podman/podman.sock`.
    fn podman_socket_paths(&self) -> Vec<String>;

    /// Whether a `docker` or `podman` client binary is on `PATH`.
    ///
    /// Distinguishes "daemon stopped" (binary present) from "not installed".
    fn client_binary_present(&self) -> bool;

    /// Whether the host looks like a rootless / user-scoped daemon setup, which
    /// selects `systemctl --user` over `sudo systemctl` in the fix action.
    fn rootless(&self) -> bool;

    /// Attempt to reach a live daemon at `endpoint` (a real API ping).
    async fn reachable(&self, endpoint: &Endpoint) -> bool;
}

/// Detects the container engine, returning one of the four [`EngineState`]s.
///
/// Never returns an error: each failing state carries its own [`EngineFix`], so
/// the caller always has a message plus one action. An explicitly-set
/// `DOCKER_HOST` is validated *before* any probing: a rejected value (see
/// `reject_docker_host`) short-circuits straight to
/// [`EngineState::UnsupportedDockerHost`] rather than falling back to the
/// Docker/Podman socket candidates, since silently ignoring the operator's
/// `DOCKER_HOST` would run the workflow against a different daemon than the
/// one they named. Otherwise, candidates are probed in the spec order —
/// `DOCKER_HOST`, then the Docker socket, then each Podman socket — and the
/// first reachable one wins.
pub async fn detect(prober: &dyn EngineProber) -> EngineState {
    if let Some(host) = prober.docker_host()
        && let Some(rejection) = reject_docker_host(&host)
    {
        return EngineState::UnsupportedDockerHost(unsupported_docker_host_fix(&host, rejection));
    }

    for endpoint in candidate_endpoints(prober) {
        if prober.reachable(&endpoint).await {
            return EngineState::Available { endpoint };
        }
    }

    if prober.client_binary_present() {
        EngineState::DaemonStopped(daemon_stopped_fix(prober.rootless()))
    } else {
        EngineState::NotInstalled(not_installed_fix())
    }
}

/// Why a `DOCKER_HOST` value is rejected outright rather than merely found
/// unreachable — shared between [`detect`] (so a rejected value is reported
/// immediately) and [`crate::docker::connect_docker_host`] (the actual
/// connector, which enforces the same rule again as defense in depth), so the
/// rule can never drift between the two call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockerHostRejection {
    /// `ssh://` — bollard's `ssh` transport is a feature v0 does not enable.
    UnsupportedTransport,
    /// A `tcp://`/`http(s)://` host that is not `localhost`/loopback.
    Remote,
}

/// Classifies a `DOCKER_HOST` value, returning the rejection reason if v0's
/// engine backend must refuse it outright (as opposed to it simply being
/// unreachable, which is a normal candidate-probing miss).
pub(crate) fn reject_docker_host(url: &str) -> Option<DockerHostRejection> {
    if url.starts_with("ssh://") {
        return Some(DockerHostRejection::UnsupportedTransport);
    }
    let is_tcp =
        url.starts_with("tcp://") || url.starts_with("http://") || url.starts_with("https://");
    if is_tcp && !docker_host_is_local(url) {
        return Some(DockerHostRejection::Remote);
    }
    None
}

/// Whether a `tcp://`/`http(s)://` `DOCKER_HOST` URL's host is `localhost` or
/// a loopback address — the only hosts where the daemon resolving a
/// bind-mount path lands on the same filesystem `litci` read the repository
/// from (<https://docs.docker.com/engine/storage/bind-mounts/>).
///
/// A host that fails to parse (empty, or not a recognized literal) is treated
/// as non-local: fail closed rather than guess.
fn docker_host_is_local(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host_and_port = after_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(after_scheme);
    let host = strip_port(host_and_port);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Strips a trailing `:port` from a `host:port` (or bracketed `[ipv6]:port`)
/// pair, leaving the bare host.
fn strip_port(host_and_port: &str) -> &str {
    if let Some(after_bracket) = host_and_port.strip_prefix('[') {
        return after_bracket.split(']').next().unwrap_or("");
    }
    match host_and_port.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => host_and_port,
    }
}

/// The fix for a `DOCKER_HOST` value [`reject_docker_host`] refused.
fn unsupported_docker_host_fix(host: &str, rejection: DockerHostRejection) -> EngineFix {
    match rejection {
        DockerHostRejection::UnsupportedTransport => EngineFix {
            message: format!(
                "DOCKER_HOST (`{host}`) uses a transport Greenlit's engine backend does not support."
            ),
            action: "point DOCKER_HOST at a unix:// socket or tcp:// endpoint, or unset it so \
                     Greenlit uses the local Docker/Podman socket"
                .to_string(),
        },
        DockerHostRejection::Remote => EngineFix {
            message: format!(
                "DOCKER_HOST (`{host}`) points at a remote daemon. Greenlit v0 targets a \
                 single local Linux x86_64 daemon and binds the repository checkout by host \
                 path, which a remote daemon would resolve on its own filesystem instead."
            ),
            action: "point DOCKER_HOST at localhost/127.0.0.1 (or a unix:// socket), or run \
                     litci directly on the machine whose daemon should build and bind the repo"
                .to_string(),
        },
    }
}

/// Builds the ordered candidate list from the prober's environment view.
fn candidate_endpoints(prober: &dyn EngineProber) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    if let Some(host) = prober.docker_host() {
        endpoints.push(Endpoint::DockerHost(host));
    }
    endpoints.push(Endpoint::DockerSocket);
    for path in prober.podman_socket_paths() {
        endpoints.push(Endpoint::PodmanSocket(path));
    }
    endpoints
}

/// The fix for an installed-but-stopped daemon.
///
/// The one action differs by scope: a rootless daemon is a per-user systemd
/// service (`systemctl --user`), a rootful one needs root (`sudo systemctl`).
/// The message names socket activation because a socket-activated install may
/// need `.socket` started rather than `.service` — the operator should know the
/// daemon is present, only asleep, never "cannot connect".
fn daemon_stopped_fix(rootless: bool) -> EngineFix {
    if rootless {
        EngineFix {
            message: "A container engine is installed but its rootless daemon is not \
                      running. If it is socket-activated, starting the user socket will \
                      wake it."
                .to_string(),
            action: "systemctl --user start docker".to_string(),
        }
    } else {
        EngineFix {
            message: "A container engine is installed but the Docker daemon is not \
                      running. If it is socket-activated, `sudo systemctl start \
                      docker.socket` will wake it on demand."
                .to_string(),
            action: "sudo systemctl start docker".to_string(),
        }
    }
}

/// The production [`EngineProber`]: reads the real environment, probes `PATH`,
/// and pings candidate endpoints through a live [`crate::DockerEngine`].
///
/// All detection I/O is confined here so the pure [`detect`] algorithm stays
/// fake-driven in tests (`PHASE-2-execution.md` exit criterion 4).
#[derive(Debug, Default)]
pub struct SystemProber;

impl SystemProber {
    /// A prober backed by the real host environment.
    #[must_use]
    pub fn new() -> Self {
        SystemProber
    }
}

/// The conventional rootful Podman socket path.
const PODMAN_ROOTFUL_SOCKET: &str = "/run/podman/podman.sock";

#[async_trait]
impl EngineProber for SystemProber {
    fn docker_host(&self) -> Option<String> {
        std::env::var("DOCKER_HOST").ok().filter(|v| !v.is_empty())
    }

    fn podman_socket_paths(&self) -> Vec<String> {
        let mut paths = vec![PODMAN_ROOTFUL_SOCKET.to_string()];
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR")
            && !runtime_dir.is_empty()
        {
            paths.push(format!("{runtime_dir}/podman/podman.sock"));
        }
        paths
    }

    fn client_binary_present(&self) -> bool {
        binary_on_path("docker") || binary_on_path("podman")
    }

    fn rootless(&self) -> bool {
        // A rootless daemon exposes its socket under the per-user runtime
        // directory; `DOCKER_HOST` pointing there, or a rootless socket
        // existing there, selects the `systemctl --user` fix.
        if self
            .docker_host()
            .is_some_and(|host| host.contains("/run/user/"))
        {
            return true;
        }
        std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|dir| !dir.is_empty())
            .is_some_and(|dir| std::path::Path::new(&format!("{dir}/docker.sock")).exists())
    }

    async fn reachable(&self, endpoint: &Endpoint) -> bool {
        // A Unix-socket endpoint whose socket file is absent is unreachable
        // without paying the client's connect timeout.
        if let Endpoint::DockerSocket = endpoint
            && !std::path::Path::new(Endpoint::DOCKER_SOCKET_PATH).exists()
        {
            return false;
        }
        if let Endpoint::PodmanSocket(path) = endpoint
            && !std::path::Path::new(path).exists()
        {
            return false;
        }
        let Ok(engine) = crate::DockerEngine::connect(endpoint) else {
            return false;
        };
        // A cheap round-trip proves the daemon actually answers.
        engine
            .image_exists("greenlit/probe:definitely-absent")
            .await
            .is_ok()
    }
}

/// Whether `name` resolves to an executable file on any `PATH` entry.
fn binary_on_path(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
    })
}

/// The fix for a host with no container engine installed.
fn not_installed_fix() -> EngineFix {
    EngineFix {
        message: "No container engine is installed. Greenlit needs Docker (or a \
                  Docker-API-compatible Podman) to run workflows."
            .to_string(),
        action: "litci setup".to_string(),
    }
}
