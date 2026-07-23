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

/// The outcome of engine detection — exactly the spec's three states.
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

/// Detects the container engine, returning one of the three [`EngineState`]s.
///
/// Never returns an error: each failing state carries its own [`EngineFix`], so
/// the caller always has a message plus one action. Candidates are probed in
/// the spec order — `DOCKER_HOST`, then the Docker socket, then each Podman
/// socket — and the first reachable one wins.
pub async fn detect(prober: &dyn EngineProber) -> EngineState {
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
