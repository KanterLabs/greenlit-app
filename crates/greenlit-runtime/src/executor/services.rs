//! The per-job network, the shim bound on its gateway, and the toolcache.
//!
//! `PHASE-4-environment.md` gives this module three jobs that all hang off one
//! resource — the job's own bridge network:
//!
//! * The **shim** (`greenlit-store`) serves `actions/cache` and the artifact
//!   actions. It binds on the bridge *gateway* rather than on every host
//!   interface, so it is reachable from the job and from nothing else, and the
//!   job addresses it at that same gateway address.
//! * The **toolcache** is a host directory bound at `RUNNER_TOOL_CACHE`, so a
//!   `setup-*` action finds a toolchain it installed on a previous run instead
//!   of downloading it again.
//! * **Service containers** attach to the same bridge (added in the next task
//!   group), which is why the network is created here rather than inside the
//!   shim setup.
//!
//! # Why the gateway, specifically
//!
//! A container cannot reach the host's loopback — `127.0.0.1` inside the
//! container is the container. What it *can* reach is the bridge's own host-
//! side address, which the daemon reports as the network's gateway. Binding
//! there satisfies the brief's "bind only on the Greenlit bridge gateway"
//! literally: the socket exists on one interface that only this run's
//! containers are attached to.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use greenlit_engine::execution::env::ActionsService;
use greenlit_store::{ArtifactStore, CacheStore, ShimState};

use crate::engine::{ContainerEngine, ContainerSpec, HealthState};
use crate::error::RuntimeError;
use crate::executor::ExecError;
use crate::progress::{ProgressEvent, ProgressSink};

/// Where the local stores live, and whether to serve them at all.
///
/// `litci plan` starts no containers and therefore no shim; a run whose store
/// roots cannot be resolved also runs without one. In both cases the
/// `ACTIONS_*` variables are simply absent, which makes `actions/cache` behave
/// as it does on a runner with no cache service — an honest miss — rather than
/// pointing it at a URL nothing answers.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// The cache store. Passed as a *store* rather than a path so the caller
    /// keeps a clone sharing its counters: the shim serves on a spawned task
    /// that does not inherit the scoped tracing subscriber, so hit/miss
    /// accounting has to travel through the store rather than through spans.
    pub cache: CacheStore,
    /// The artifact store.
    pub artifacts: ArtifactStore,
    /// `~/.litci/toolcache`, bound into the job at `RUNNER_TOOL_CACHE`.
    pub toolcache_root: PathBuf,
    /// Greenlit-owned dependency download caches. Installs still run; only
    /// immutable/downloaded package material is reused.
    pub package_cache_root: PathBuf,
    /// The bearer token this run's shim requires on its API routes.
    pub runtime_token: String,
    /// The per-run signature blob URLs carry in place of a bearer header.
    ///
    /// Separate from the token because blob clients never send a header and
    /// the URL therefore has to authorize itself; keeping them distinct means
    /// the bearer token never appears in a URL a client might log.
    pub url_signature: String,
}

impl StoreConfig {
    /// The `~/.litci` root every store sits under.
    ///
    /// Derived from the toolcache path rather than carried separately so the
    /// two can never disagree about where user-local state lives.
    #[must_use]
    pub fn toolcache_root_parent(&self) -> PathBuf {
        self.toolcache_root
            .parent()
            .map_or_else(|| self.toolcache_root.clone(), Path::to_path_buf)
    }
}

/// The per-job network and everything bound to it.
///
/// Dropping this stops the shim; [`JobNetwork::teardown`] additionally removes
/// the network, which cannot happen until every container on it is gone.
pub struct JobNetwork {
    /// The network's name, for attaching containers.
    name: String,
    /// The shim, if one is being served.
    shim: Option<greenlit_store::Shim>,
    /// The `ACTIONS_*` values the job's environment needs.
    service: Option<ActionsService>,
    /// What the network policy needs to know about this bridge.
    policy: crate::executor::netguard::Policy,
}

impl JobNetwork {
    /// The network name containers attach to.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `ACTIONS_*` environment for this job, if a shim is running.
    #[must_use]
    pub fn actions_service(&self) -> Option<&ActionsService> {
        self.service.as_ref()
    }

    /// The network policy to apply inside every container on this bridge.
    #[must_use]
    pub fn policy(&self) -> &crate::executor::netguard::Policy {
        &self.policy
    }

    /// Stops the shim and removes the network.
    ///
    /// Best-effort: a leaked network is not a run failure and must never mask
    /// the job's real result. Ordering matters — the network cannot be removed
    /// while a container is still attached, so callers tear containers down
    /// first.
    pub async fn teardown(mut self, engine: &dyn ContainerEngine) {
        if let Some(shim) = self.shim.take() {
            shim.shutdown().await;
        }
        let _ = engine.remove_network(&self.name).await;
    }
}

/// Creates the job's network and, when `store` is present, binds the shim on
/// its gateway.
///
/// # Errors
/// Returns [`RuntimeError::Api`] if the network cannot be created or
/// inspected. A shim that cannot bind is *not* an error: the run continues
/// without a cache service, which is a degraded but honest state, where
/// failing would deny a workflow that never touches the cache.
pub async fn create(
    engine: &dyn ContainerEngine,
    name: &str,
    store: Option<&StoreConfig>,
) -> Result<JobNetwork, RuntimeError> {
    engine.create_network(name).await?;
    let info = engine.inspect_network(name).await?;

    let base_policy = crate::executor::netguard::Policy {
        gateway: info.gateway.clone(),
        subnet: info.subnet.clone(),
        shim_port: None,
    };

    let Some(store) = store else {
        return Ok(JobNetwork {
            name: name.to_string(),
            shim: None,
            service: None,
            policy: base_policy,
        });
    };

    // Without a gateway there is no address the container could reach the
    // shim at, so there is nothing to bind.
    let Some(gateway) = info.gateway.as_deref().and_then(parse_ipv4) else {
        tracing::debug!(
            target: "greenlit_runtime::services",
            network = name,
            "the job network reported no IPv4 gateway; running without a cache service"
        );
        return Ok(JobNetwork {
            name: name.to_string(),
            shim: None,
            service: None,
            policy: base_policy,
        });
    };

    let Ok(bound) = greenlit_store::bind(gateway).await else {
        tracing::debug!(
            target: "greenlit_runtime::services",
            %gateway,
            "could not bind the cache shim; running without a cache service"
        );
        return Ok(JobNetwork {
            name: name.to_string(),
            shim: None,
            service: None,
            policy: base_policy,
        });
    };

    // The base URL has to carry the port the kernel just chose, and the
    // client concatenates onto it, so the trailing slash is load-bearing.
    let port = bound.address().port();
    let base = format!("http://{gateway}:{port}/");
    let state = ShimState::new(
        store.cache.clone(),
        store.artifacts.clone(),
        store.runtime_token.clone(),
        store.url_signature.clone(),
        base.clone(),
    );
    let shim = bound.serve(state);

    Ok(JobNetwork {
        name: name.to_string(),
        shim: Some(shim),
        policy: crate::executor::netguard::Policy {
            shim_port: Some(port),
            ..base_policy
        },
        service: Some(ActionsService {
            cache_url: base.clone(),
            results_url: base,
            runtime_token: store.runtime_token.clone(),
        }),
    })
}

/// How long a service has to report healthy before the job gives up.
///
/// GitHub's runner waits on a service's own probe with no separate deadline
/// of its own; Greenlit adds one so a mis-authored `--health-cmd` fails with
/// a message naming the service instead of hanging the run forever.
const HEALTH_DEADLINE: Duration = Duration::from_secs(120);

/// How often the health gate polls.
const HEALTH_POLL: Duration = Duration::from_millis(500);

/// A service that failed to start, plus the ones already running.
///
/// The caller needs both: the cause to report, and the started set to stop
/// before it can remove the network they are attached to.
pub struct ServiceStartFailure {
    /// Why the start failed.
    pub cause: ExecError,
    /// Services that started before the failure.
    pub started: Vec<StartedService>,
}

/// One started service container.
pub struct StartedService {
    /// The service id, which is also its hostname on the job network.
    pub id: String,
    /// The container id, for teardown.
    pub container: String,
}

/// Starts `services` on `network`, waiting for each to report healthy.
///
/// Each service is reachable from the job at its service id
/// (`PHASE-4-environment.md`: "hostname = service key"), which is what a
/// workflow's `postgres:5432` connection string relies on.
///
/// # Errors
/// Returns [`ServiceStartFailure`] if a service cannot be pulled, created, or
/// started, or if its health probe does not pass within [`HEALTH_DEADLINE`].
/// A service that never becomes healthy fails the job rather than letting the
/// first step discover it by timing out on a connection.
///
/// The failure carries the services that *did* start, because the caller has
/// to stop them before it can remove the network they are attached to.
pub async fn start(
    engine: &dyn ContainerEngine,
    config: &crate::executor::RunConfig,
    network: &str,
    policy: &crate::executor::netguard::Policy,
    services: &[(String, ResolvedService)],
    progress: &mut (dyn ProgressSink + Send),
) -> Result<Vec<StartedService>, ServiceStartFailure> {
    let mut started = Vec::new();
    for (id, service) in services {
        progress.on_progress(ProgressEvent::ServiceStarting {
            service: id.clone(),
        });

        macro_rules! bail {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(cause) => {
                        return Err(ServiceStartFailure {
                            cause: cause.into(),
                            started,
                        });
                    }
                }
            };
        }

        let image = bail!(config.locked_image(&service.image));
        if !bail!(engine.image_exists(&image).await) {
            if config.locked_images.is_some() {
                return Err(ServiceStartFailure {
                    cause: crate::executor::ExecError::Infrastructure {
                        message: format!(
                            "locked service image '{image}' disappeared before service startup"
                        ),
                        fix: "retry to prepare the exact image again; do not prune Docker images during an active run"
                            .to_string(),
                    },
                    started,
                });
            }
            bail!(
                engine
                    .pull_image(&image, service.registry_auth.as_ref(), progress)
                    .await
            );
        }

        let spec = ContainerSpec {
            image,
            env: service.env.clone(),
            network: Some(network.to_string()),
            // The service id is the name the job connects to.
            network_aliases: vec![id.clone()],
            hostname: Some(id.clone()),
            ports: service.ports.clone(),
            healthcheck: service.healthcheck.clone(),
            binds: service.volume_binds.clone(),
            labels: vec![
                ("greenlit.managed".to_string(), "1".to_string()),
                ("greenlit.service".to_string(), id.clone()),
            ],
            ..ContainerSpec::default()
        };

        let container = bail!(engine.create_container(&spec).await);
        started.push(StartedService {
            id: id.clone(),
            container: container.clone(),
        });
        bail!(engine.start_container(&container).await);
        // A service is untrusted code too, and it is on the same bridge as
        // the job -- so it is contained by the same policy.
        let netguard_image = bail!(config.locked_image(crate::executor::netguard::NETGUARD_IMAGE));
        if config.locked_images.is_some() && !bail!(engine.image_exists(&netguard_image).await) {
            return Err(ServiceStartFailure {
                cause: crate::executor::ExecError::Infrastructure {
                    message: format!(
                        "locked network-guard image '{netguard_image}' disappeared before service startup"
                    ),
                    fix: "retry to prepare the exact image again; do not prune Docker images during an active run"
                        .to_string(),
                },
                started,
            });
        }
        bail!(
            crate::executor::netguard::apply(
                engine,
                &container,
                policy,
                &netguard_image,
                progress,
            )
            .await
        );
        bail!(wait_healthy(engine, &container, id).await);
        progress.on_progress(ProgressEvent::ServiceReady {
            service: id.clone(),
        });
    }
    Ok(started)
}

/// Polls until the service reports healthy, or fails with a message naming it.
///
/// A service with no configured probe is ready as soon as its container is
/// running — the same rule the runner applies, and the reason
/// [`crate::engine::HealthState::None`] is distinct from `Starting`.
async fn wait_healthy(
    engine: &dyn ContainerEngine,
    container: &str,
    id: &str,
) -> Result<(), ExecError> {
    let deadline = Instant::now() + HEALTH_DEADLINE;
    loop {
        let state = engine.inspect_container(container).await?;
        match state.health {
            HealthState::None if state.running => return Ok(()),
            HealthState::Healthy => return Ok(()),
            HealthState::Unhealthy => {
                return Err(ExecError::Infrastructure {
                    message: format!("service `{id}` reported unhealthy"),
                    fix: format!(
                        "check the service image and its `--health-cmd`; run `docker logs` against the `greenlit.service={id}` container to see why the probe failed"
                    ),
                });
            }
            _ if !state.running => {
                return Err(ExecError::Infrastructure {
                    message: format!("service `{id}` exited before becoming ready"),
                    fix: "check the service image's own configuration — a service that exits immediately usually needs `env:` it was not given".to_string(),
                });
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "service `{id}` did not become healthy within {}s",
                    HEALTH_DEADLINE.as_secs()
                ),
                fix: "raise `--health-start-period`/`--health-retries`, or check that the probe command is right for the image".to_string(),
            });
        }
        tokio::time::sleep(HEALTH_POLL).await;
    }
}

/// Removes every started service container, best effort.
pub async fn stop(engine: &dyn ContainerEngine, services: &[StartedService]) {
    for service in services {
        // A leaked service container is not a run failure and must never mask
        // the job's real result, so a removal failure is recorded rather than
        // propagated.
        if engine.remove_container(&service.container).await.is_err() {
            tracing::debug!(
                target: "greenlit_runtime::services",
                service = service.id,
                "could not remove the service container"
            );
        }
    }
}

/// A service resolved and validated, ready to start.
#[derive(Debug, Clone, Default)]
pub struct ResolvedService {
    /// The image reference.
    pub image: String,
    /// Resolved `env:`.
    pub env: Vec<(String, String)>,
    /// Ports to publish.
    pub ports: Vec<crate::engine::PortBinding>,
    /// Named-volume binds.
    pub volume_binds: Vec<crate::engine::BindMount>,
    /// The probe from `--health-*`, if any.
    pub healthcheck: Option<crate::engine::HealthCheck>,
    /// Private-registry credentials, if authored.
    pub registry_auth: Option<crate::engine::RegistryAuth>,
}

/// The host bind that makes `RUNNER_TOOL_CACHE` persist across runs.
///
/// Unlike every other host bind Greenlit creates this one is writable, which
/// is the point: a `setup-*` action installs a toolchain into it and the next
/// run finds it already there. It is a Greenlit-owned directory under
/// `~/.litci`, never any part of the user's checkout, so a workflow writing
/// into it cannot reach the working tree.
///
/// # Errors
/// Returns the directory-creation failure. The caller treats that as
/// "no toolcache this run" rather than as a run failure.
pub fn toolcache_bind(
    root: &std::path::Path,
    container_path: &str,
) -> std::io::Result<crate::engine::BindMount> {
    std::fs::create_dir_all(root)?;
    Ok(crate::engine::BindMount {
        host_path: root.to_string_lossy().into_owned(),
        container_path: container_path.to_string(),
        read_only: false,
    })
}

/// Writable Cargo download-cache binds for common runner and official Rust
/// container layouts. Build outputs are deliberately not included.
pub fn cargo_download_binds(
    root: &std::path::Path,
) -> std::io::Result<Vec<crate::engine::BindMount>> {
    let registry = root.join("cargo").join("registry");
    let git = root.join("cargo").join("git");
    std::fs::create_dir_all(&registry)?;
    std::fs::create_dir_all(&git)?;
    let registry = registry.to_string_lossy().into_owned();
    let git = git.to_string_lossy().into_owned();
    Ok(vec![
        crate::engine::BindMount {
            host_path: registry.clone(),
            container_path: "/usr/local/cargo/registry".to_string(),
            read_only: false,
        },
        crate::engine::BindMount {
            host_path: git.clone(),
            container_path: "/usr/local/cargo/git".to_string(),
            read_only: false,
        },
        crate::engine::BindMount {
            host_path: registry,
            container_path: "/home/runner/.cargo/registry".to_string(),
            read_only: false,
        },
        crate::engine::BindMount {
            host_path: git,
            container_path: "/home/runner/.cargo/git".to_string(),
            read_only: false,
        },
    ])
}

/// Parses a gateway address, ignoring any `/prefix` the daemon appends.
fn parse_ipv4(value: &str) -> Option<Ipv4Addr> {
    value
        .split('/')
        .next()
        .and_then(|address| address.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::parse_ipv4;

    #[test]
    fn a_gateway_parses_with_or_without_a_prefix() {
        assert_eq!(
            parse_ipv4("172.18.0.1"),
            Some(std::net::Ipv4Addr::new(172, 18, 0, 1))
        );
        // Docker reports some gateways in CIDR form.
        assert_eq!(
            parse_ipv4("172.18.0.1/16"),
            Some(std::net::Ipv4Addr::new(172, 18, 0, 1))
        );
        // An IPv6 gateway is not one this run can bind on.
        assert_eq!(parse_ipv4("fd00::1"), None);
        assert_eq!(parse_ipv4(""), None);
    }
}
