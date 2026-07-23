//! Engine-detection state machine, exercised across all three spec states with
//! an injected fake prober — no real Docker daemon.
//!
//! `PHASE-2-execution.md` exit criterion 4: "Engine detection behavior covers
//! all three states through integration tests with the injected external
//! prober." `TESTING.md` classes the engine prober as a true external boundary
//! that is mocked here; the detection algorithm itself runs real.

use async_trait::async_trait;

use greenlit_runtime::detect;
use greenlit_runtime::detect::{Endpoint, EngineProber, EngineState};

/// A scripted [`EngineProber`]: it answers from fixed data instead of touching
/// the environment, filesystem, or a socket.
#[derive(Default)]
struct FakeProber {
    docker_host: Option<String>,
    podman_socket_paths: Vec<String>,
    reachable: Vec<Endpoint>,
    binary_present: bool,
    rootless: bool,
}

#[async_trait]
impl EngineProber for FakeProber {
    fn docker_host(&self) -> Option<String> {
        self.docker_host.clone()
    }

    fn podman_socket_paths(&self) -> Vec<String> {
        self.podman_socket_paths.clone()
    }

    fn client_binary_present(&self) -> bool {
        self.binary_present
    }

    fn rootless(&self) -> bool {
        self.rootless
    }

    async fn reachable(&self, endpoint: &Endpoint) -> bool {
        self.reachable.contains(endpoint)
    }
}

// --- State (a): reachable ------------------------------------------------

#[tokio::test]
async fn docker_host_wins_over_socket_in_detection_order() {
    // Both DOCKER_HOST and the Docker socket answer; the spec's order
    // (DOCKER_HOST first) must select DOCKER_HOST. A local (loopback)
    // DOCKER_HOST is used here — v0 rejects a non-local one outright
    // (`docker_host_is_rejected_before_any_probing_when_it_points_remote`),
    // which is a distinct behavior from this test's "which candidate wins"
    // concern.
    let prober = FakeProber {
        docker_host: Some("tcp://127.0.0.1:2375".to_string()),
        reachable: vec![
            Endpoint::DockerHost("tcp://127.0.0.1:2375".to_string()),
            Endpoint::DockerSocket,
        ],
        ..Default::default()
    };
    let state = detect(&prober).await;
    assert_eq!(
        state,
        EngineState::Available {
            endpoint: Endpoint::DockerHost("tcp://127.0.0.1:2375".to_string()),
        }
    );
}

// --- DOCKER_HOST rejection (finding: a remote daemon binds the wrong host
// path for the repository) ---------------------------------------------

#[tokio::test]
async fn docker_host_is_rejected_before_any_probing_when_it_points_remote() {
    // A non-local host is refused immediately — it must never fall back to
    // the local Docker socket even when that socket also answers, since that
    // would silently ignore the operator's explicit `DOCKER_HOST`.
    let prober = FakeProber {
        docker_host: Some("tcp://10.0.0.2:2375".to_string()),
        reachable: vec![
            Endpoint::DockerHost("tcp://10.0.0.2:2375".to_string()),
            Endpoint::DockerSocket,
        ],
        ..Default::default()
    };
    match detect(&prober).await {
        EngineState::UnsupportedDockerHost(fix) => {
            assert!(fix.message.contains("remote daemon"));
            assert!(fix.action.contains("localhost"));
        }
        other => panic!("expected UnsupportedDockerHost, got {other:?}"),
    }
}

#[tokio::test]
async fn docker_host_ssh_transport_is_rejected() {
    let prober = FakeProber {
        docker_host: Some("ssh://build-box".to_string()),
        ..Default::default()
    };
    match detect(&prober).await {
        EngineState::UnsupportedDockerHost(fix) => {
            assert!(fix.action.contains("unix://") || fix.action.contains("tcp://"));
        }
        other => panic!("expected UnsupportedDockerHost, got {other:?}"),
    }
}

#[tokio::test]
async fn docker_host_loopback_variants_are_accepted_by_detection() {
    for host in [
        "tcp://localhost:2375",
        "tcp://127.0.0.1:2375",
        "tcp://[::1]:2375",
        "http://127.0.0.1:2375",
    ] {
        let prober = FakeProber {
            docker_host: Some(host.to_string()),
            reachable: vec![Endpoint::DockerHost(host.to_string())],
            ..Default::default()
        };
        assert_eq!(
            detect(&prober).await,
            EngineState::Available {
                endpoint: Endpoint::DockerHost(host.to_string()),
            },
            "expected {host} to be accepted as local"
        );
    }
}

#[tokio::test]
async fn falls_back_to_docker_socket_when_no_docker_host() {
    let prober = FakeProber {
        reachable: vec![Endpoint::DockerSocket],
        ..Default::default()
    };
    let state = detect(&prober).await;
    assert_eq!(
        state,
        EngineState::Available {
            endpoint: Endpoint::DockerSocket
        }
    );
}

#[tokio::test]
async fn falls_back_to_podman_socket_after_docker() {
    // Docker socket is unreachable; the rootless Podman socket answers.
    let podman = "/run/user/1000/podman/podman.sock".to_string();
    let prober = FakeProber {
        podman_socket_paths: vec![podman.clone()],
        reachable: vec![Endpoint::PodmanSocket(podman.clone())],
        ..Default::default()
    };
    let state = detect(&prober).await;
    assert_eq!(
        state,
        EngineState::Available {
            endpoint: Endpoint::PodmanSocket(podman)
        }
    );
}

// --- State (b): installed but daemon stopped -----------------------------

#[tokio::test]
async fn rootful_daemon_stopped_offers_sudo_systemctl() {
    let prober = FakeProber {
        binary_present: true,
        rootless: false,
        ..Default::default()
    };
    match detect(&prober).await {
        EngineState::DaemonStopped(fix) => {
            assert_eq!(fix.action, "sudo systemctl start docker");
            // The message must explain the daemon is present, not raise a raw
            // connection error (UX invariant), and mention socket activation.
            assert!(fix.message.contains("installed"));
            assert!(fix.message.to_lowercase().contains("socket-activated"));
        }
        other => panic!("expected DaemonStopped, got {other:?}"),
    }
}

#[tokio::test]
async fn rootless_daemon_stopped_offers_user_systemctl() {
    let prober = FakeProber {
        binary_present: true,
        rootless: true,
        ..Default::default()
    };
    match detect(&prober).await {
        EngineState::DaemonStopped(fix) => {
            assert_eq!(fix.action, "systemctl --user start docker");
        }
        other => panic!("expected DaemonStopped, got {other:?}"),
    }
}

// --- State (c): absent ----------------------------------------------------

#[tokio::test]
async fn no_engine_points_at_litci_setup() {
    let prober = FakeProber::default();
    match detect(&prober).await {
        EngineState::NotInstalled(fix) => {
            assert_eq!(fix.action, "litci setup");
        }
        other => panic!("expected NotInstalled, got {other:?}"),
    }
}
