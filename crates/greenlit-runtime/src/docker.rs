//! The bollard-backed [`ContainerEngine`] implementation.
//!
//! `AGENTS.md` ("Tech"): "Docker via API (bollard crate), no shelling out to
//! `docker`." Every method here is a Docker Engine API call through
//! [`bollard::Docker`]; nothing execs the `docker` binary. Podman's
//! Docker-compatible socket is spoken to by the same client, so a
//! [`Endpoint::PodmanSocket`] needs no special case beyond the connection.

use std::collections::HashMap;

use async_trait::async_trait;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{
    ContainerConfig, ContainerCreateBody, CreateImageInfo, EndpointSettings, HealthConfig,
    HealthStatusEnum, HostConfig, NetworkCreateRequest, NetworkingConfig,
    PortBinding as BollardPortBinding, VolumeCreateRequest,
};
use bollard::query_parameters::{
    BuildImageOptionsBuilder, CommitContainerOptionsBuilder, CreateContainerOptionsBuilder,
    CreateImageOptionsBuilder, DownloadFromContainerOptionsBuilder, InspectContainerOptions,
    ListImagesOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    RemoveImageOptions, RemoveVolumeOptions, StopContainerOptionsBuilder,
    WaitContainerOptionsBuilder,
};
use bytes::Bytes;
use futures_util::StreamExt;

use crate::detect::{DockerHostRejection, Endpoint, reject_docker_host};
use crate::engine::{
    BuildSpec, CommitSpec, ContainerEngine, ContainerSpec, ContainerState, ExecOutput,
    ExecOutputSink, ExecSpec, HealthState, ImageIdentity, ImageSummary, NetworkInfo, RegistryAuth,
};
use crate::error::{Operation, RuntimeError};
use crate::progress::{ProgressEvent, ProgressSink};

/// Connection timeout for the Docker API client, in seconds — matches bollard's
/// own default so behaviour is unsurprising.
const CONNECT_TIMEOUT_SECS: u64 = 120;

/// Grace period (seconds) given to a container to stop before the daemon kills
/// it. GitHub's runner tears jobs down promptly; a short grace keeps teardown
/// snappy.
const STOP_GRACE_SECS: i32 = 10;

/// A [`ContainerEngine`] backed by a live Docker Engine API connection.
#[derive(Clone)]
pub struct DockerEngine {
    docker: Docker,
}

impl DockerEngine {
    /// Connects a Docker API client to a reached [`Endpoint`].
    ///
    /// Detection ([`crate::detect::detect`]) has already confirmed the endpoint
    /// answers; this opens the typed client against it. Podman sockets connect
    /// exactly like Docker sockets.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Connect`] if the client cannot be constructed,
    /// or [`RuntimeError::UnsupportedDockerHost`] for a `DOCKER_HOST` transport
    /// v0 does not support (e.g. `ssh://`).
    pub fn connect(endpoint: &Endpoint) -> Result<Self, RuntimeError> {
        let docker = match endpoint {
            Endpoint::DockerHost(url) => connect_docker_host(url)?,
            Endpoint::DockerSocket => Docker::connect_with_unix(
                Endpoint::DOCKER_SOCKET_PATH,
                CONNECT_TIMEOUT_SECS,
                bollard::API_DEFAULT_VERSION,
            )
            .map_err(|source| RuntimeError::Connect {
                endpoint: endpoint.describe(),
                source,
            })?,
            Endpoint::PodmanSocket(path) => {
                Docker::connect_with_unix(path, CONNECT_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
                    .map_err(|source| RuntimeError::Connect {
                        endpoint: endpoint.describe(),
                        source,
                    })?
            }
        };
        Ok(DockerEngine { docker })
    }
}

/// Dispatches a `DOCKER_HOST` URL to the matching bollard connector by scheme.
///
/// v0 supports the local-first transports (`unix://`, `tcp://`/`http(s)://`
/// against `localhost`/a loopback address). `ssh://`, and a non-local
/// `tcp://`/`http(s)://` host, are rejected outright — [`reject_docker_host`]
/// is the single shared rule ([`crate::detect::detect`] enforces the same
/// classification before even probing reachability, so a rejected value is
/// reported immediately rather than silently falling back to the local
/// socket; this is defense in depth for anyone constructing a [`DockerEngine`]
/// directly).
fn connect_docker_host(url: &str) -> Result<Docker, RuntimeError> {
    let describe = Endpoint::DockerHost(url.to_string()).describe();
    match reject_docker_host(url) {
        Some(DockerHostRejection::UnsupportedTransport) => {
            return Err(RuntimeError::UnsupportedDockerHost {
                value: url.to_string(),
                fix: "point DOCKER_HOST at a unix:// socket or tcp:// endpoint, or unset it \
                      so Greenlit uses the local Docker/Podman socket"
                    .to_string(),
            });
        }
        Some(DockerHostRejection::Remote) => {
            return Err(RuntimeError::RemoteDockerHost {
                value: url.to_string(),
                fix: "point DOCKER_HOST at localhost/127.0.0.1 (or a unix:// socket), or run \
                      litci directly on the machine whose daemon should build and bind the repo"
                    .to_string(),
            });
        }
        None => {}
    }
    let is_tcp =
        url.starts_with("tcp://") || url.starts_with("http://") || url.starts_with("https://");
    let result = if is_tcp {
        Docker::connect_with_http(url, CONNECT_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
    } else {
        // `unix://<path>` or a bare socket path; `connect_with_unix` strips the
        // scheme itself.
        Docker::connect_with_unix(url, CONNECT_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
    };
    result.map_err(|source| RuntimeError::Connect {
        endpoint: describe,
        source,
    })
}

/// Splits an image reference into `(name, tag-or-digest)`.
///
/// Docker's `/images/create` endpoint takes `fromImage` and `tag` as separate
/// query parameters. A digest reference uses the repository before `@` and
/// the complete `sha256:...` value as the second parameter. Otherwise the tag
/// is the segment after the final `:` unless that segment contains a `/`
/// (which means the `:` was a registry-host port, e.g.
/// `localhost:5000/img`). A reference with no tag defaults to `latest`.
fn split_reference(image: &str) -> (&str, &str) {
    if let Some((name, digest)) = image.rsplit_once('@') {
        return (name, digest);
    }
    match image.rfind(':') {
        Some(idx) if !image[idx + 1..].contains('/') => (&image[..idx], &image[idx + 1..]),
        _ => (image, "latest"),
    }
}

/// Folds one pull-stream item into the per-layer byte tally and returns the
/// cumulative totals: bytes received across all layers, and the total across
/// all layers once every known layer has reported one.
///
/// A layer whose `status` reports completion ("Pull complete", "Already
/// exists") pins its `current` to its `total` — the daemon stops sending
/// byte counts for finished layers.
fn fold_pull_progress(
    layers: &mut HashMap<String, (u64, Option<u64>)>,
    info: &CreateImageInfo,
) -> (u64, Option<u64>) {
    if let Some(id) = &info.id {
        let entry = layers.entry(id.clone()).or_insert((0, None));
        if let Some(detail) = &info.progress_detail {
            if let Some(current) = detail.current {
                entry.0 = u64::try_from(current).unwrap_or(0);
            }
            if let Some(total) = detail.total {
                entry.1 = Some(u64::try_from(total).unwrap_or(0));
            }
        }
        let complete = matches!(
            info.status.as_deref(),
            Some("Pull complete" | "Already exists" | "Download complete")
        );
        if complete && let Some(total) = entry.1 {
            entry.0 = total;
        }
    }
    let current: u64 = layers.values().map(|(current, _)| current).sum();
    let total = layers
        .values()
        .map(|(_, total)| *total)
        .collect::<Option<Vec<u64>>>()
        .map(|totals| totals.iter().sum());
    (current, total)
}

#[async_trait]
impl ContainerEngine for DockerEngine {
    async fn pull_image(
        &self,
        image: &str,
        auth: Option<&RegistryAuth>,
        progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError> {
        let (name, tag) = split_reference(image);
        let options = CreateImageOptionsBuilder::new()
            .from_image(name)
            .tag(tag)
            .build();
        progress.on_progress(ProgressEvent::PullStarted {
            image: image.to_string(),
        });
        let credentials = auth.map(|auth| bollard::auth::DockerCredentials {
            username: Some(auth.username.clone()),
            password: Some(auth.password.clone()),
            ..Default::default()
        });
        let mut layers: HashMap<String, (u64, Option<u64>)> = HashMap::new();
        let mut stream = self.docker.create_image(Some(options), None, credentials);
        while let Some(item) = stream.next().await {
            let info = item.map_err(|e| RuntimeError::api(Operation::PullImage, e))?;
            let (current_bytes, total_bytes) = fold_pull_progress(&mut layers, &info);
            progress.on_progress(ProgressEvent::PullProgress {
                current_bytes,
                total_bytes,
            });
        }
        let downloaded_bytes = layers.values().map(|(current, _)| current).sum();
        progress.on_progress(ProgressEvent::PullFinished {
            image: image.to_string(),
            downloaded_bytes,
        });
        Ok(())
    }

    async fn image_exists(&self, image: &str) -> Result<bool, RuntimeError> {
        match self.docker.inspect_image(image).await {
            Ok(_) => Ok(true),
            // A 404 is the daemon's "no such image" — the expected first-use
            // state, reported as absence rather than an error.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(RuntimeError::api(Operation::InspectImage, e)),
        }
    }

    async fn image_identity(&self, image: &str) -> Result<Option<ImageIdentity>, RuntimeError> {
        let inspected = match self.docker.inspect_image(image).await {
            Ok(inspected) => inspected,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(None),
            Err(error) => return Err(RuntimeError::api(Operation::InspectImage, error)),
        };
        let digest = inspected
            .repo_digests
            .and_then(|digests| digests.into_iter().next())
            .and_then(|name| name.rsplit_once('@').map(|(_, digest)| digest.to_string()))
            .or(inspected.id);
        Ok(digest.map(|digest| ImageIdentity {
            digest,
            os: inspected.os.unwrap_or_else(|| "unknown".to_string()),
            architecture: inspected
                .architecture
                .unwrap_or_else(|| "unknown".to_string()),
        }))
    }

    async fn build_image(
        &self,
        spec: &BuildSpec,
        progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError> {
        let build_args: HashMap<String, String> = spec.build_args.iter().cloned().collect();
        let options = BuildImageOptionsBuilder::new()
            .dockerfile(&spec.dockerfile)
            .t(&spec.tag)
            .buildargs(&build_args)
            .rm(true)
            .build();
        let body = bollard::body_full(Bytes::from(spec.context_tar.clone()));
        progress.on_progress(ProgressEvent::BuildStarted {
            tag: spec.tag.clone(),
        });
        let mut stream = self.docker.build_image(options, None, Some(body));
        while let Some(item) = stream.next().await {
            let info = item.map_err(|e| RuntimeError::api(Operation::BuildImage, e))?;
            if let Some(text) = &info.stream {
                for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
                    progress.on_progress(ProgressEvent::BuildLine {
                        line: line.to_string(),
                    });
                }
            }
        }
        progress.on_progress(ProgressEvent::BuildFinished {
            tag: spec.tag.clone(),
        });
        Ok(())
    }

    async fn commit_container(&self, spec: &CommitSpec) -> Result<String, RuntimeError> {
        let options = CommitContainerOptionsBuilder::new()
            .container(&spec.container)
            .repo(&spec.repo)
            .tag(&spec.tag)
            .build();
        let response = self
            .docker
            .commit_container(options, ContainerConfig::default())
            .await
            .map_err(|e| RuntimeError::api(Operation::CommitContainer, e))?;
        Ok(response.id)
    }

    async fn create_container(&self, spec: &ContainerSpec) -> Result<String, RuntimeError> {
        let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let labels: HashMap<String, String> = spec.labels.iter().cloned().collect();
        // Bollard expresses binds as `host:container[:ro]` strings on
        // `HostConfig.binds`. Greenlit only ever produces the read-only repo
        // lower layer here, but the flag is honoured generically.
        let binds: Vec<String> = spec
            .binds
            .iter()
            .map(|bind| {
                let mode = if bind.read_only { ":ro" } else { ":rw" };
                format!("{}:{}{mode}", bind.host_path, bind.container_path)
            })
            .collect();
        // Published ports are expressed twice: `exposed_ports` on the config
        // declares them, `port_bindings` on the host config says where they
        // land. Greenlit always sets `host_ip`, so a service is reachable from
        // the job and from nowhere else on the host.
        let mut exposed_ports: Vec<String> = Vec::new();
        let mut port_bindings: HashMap<String, Option<Vec<BollardPortBinding>>> = HashMap::new();
        for port in &spec.ports {
            let key = format!("{}/{}", port.container_port, port.protocol);
            if !exposed_ports.contains(&key) {
                exposed_ports.push(key.clone());
            }
            port_bindings
                .entry(key)
                .or_insert_with(|| Some(Vec::new()))
                .get_or_insert_with(Vec::new)
                .push(BollardPortBinding {
                    host_ip: port.host_ip.clone(),
                    host_port: port.host_port.map(|value| value.to_string()),
                });
        }

        let needs_host_config = spec.network.is_some()
            || !binds.is_empty()
            || !spec.cap_add.is_empty()
            || !port_bindings.is_empty()
            || spec.privileged;
        let host_config = needs_host_config.then(|| HostConfig {
            network_mode: spec.network.clone(),
            binds: (!binds.is_empty()).then_some(binds),
            privileged: spec.privileged.then_some(true),
            cap_add: (!spec.cap_add.is_empty()).then(|| spec.cap_add.clone()),
            port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
            ..Default::default()
        });

        // A network alias only applies to a *named* network, not to the
        // `container:<id>` namespace-sharing mode the netguard sidecar uses,
        // where the endpoint config has no meaning.
        let networking_config = match (&spec.network, spec.network_aliases.is_empty()) {
            (Some(network), false) if !network.starts_with("container:") => {
                let endpoint = EndpointSettings {
                    aliases: Some(spec.network_aliases.clone()),
                    ..Default::default()
                };
                Some(NetworkingConfig {
                    endpoints_config: Some(HashMap::from([(network.clone(), endpoint)])),
                })
            }
            _ => None,
        };

        let body = ContainerCreateBody {
            image: Some(spec.image.clone()),
            hostname: spec.hostname.clone(),
            entrypoint: (!spec.entrypoint.is_empty()).then(|| spec.entrypoint.clone()),
            cmd: (!spec.cmd.is_empty()).then(|| spec.cmd.clone()),
            env: (!env.is_empty()).then_some(env),
            working_dir: spec.working_dir.clone(),
            labels: (!labels.is_empty()).then_some(labels),
            exposed_ports: (!exposed_ports.is_empty()).then_some(exposed_ports),
            healthcheck: spec.healthcheck.as_ref().map(|health| HealthConfig {
                test: (!health.test.is_empty()).then(|| health.test.clone()),
                interval: health.interval_nanos,
                timeout: health.timeout_nanos,
                retries: health.retries,
                start_period: health.start_period_nanos,
                ..Default::default()
            }),
            networking_config,
            host_config,
            ..Default::default()
        };
        let options = spec
            .name
            .as_ref()
            .map(|name| CreateContainerOptionsBuilder::new().name(name).build());
        let response = self
            .docker
            .create_container(options, body)
            .await
            .map_err(|e| RuntimeError::api(Operation::CreateContainer, e))?;
        Ok(response.id)
    }

    async fn start_container(&self, id: &str) -> Result<(), RuntimeError> {
        self.docker
            .start_container(id, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .map_err(|e| RuntimeError::api(Operation::StartContainer, e))
    }

    async fn inspect_container(&self, id: &str) -> Result<ContainerState, RuntimeError> {
        let response = self
            .docker
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| RuntimeError::api(Operation::InspectContainer, e))?;
        let state = response.state;
        // A container with no configured probe reports `None`/`Empty` rather
        // than a status, which the health gate must read as "nothing to wait
        // for" -- not as "not healthy yet".
        let health = state
            .as_ref()
            .and_then(|state| state.health.as_ref())
            .and_then(|health| health.status)
            .map_or(HealthState::None, |status| match status {
                HealthStatusEnum::HEALTHY => HealthState::Healthy,
                HealthStatusEnum::UNHEALTHY => HealthState::Unhealthy,
                HealthStatusEnum::STARTING => HealthState::Starting,
                HealthStatusEnum::NONE | HealthStatusEnum::EMPTY => HealthState::None,
            });
        Ok(ContainerState {
            running: state.as_ref().and_then(|state| state.running) == Some(true),
            exit_code: state.as_ref().and_then(|state| state.exit_code),
            health,
        })
    }

    async fn create_volume(&self, name: &str) -> Result<(), RuntimeError> {
        self.docker
            .create_volume(VolumeCreateRequest {
                name: Some(name.to_string()),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(|e| RuntimeError::api(Operation::CreateVolume, e))
    }

    async fn remove_volume(&self, name: &str) -> Result<(), RuntimeError> {
        match self
            .docker
            .remove_volume(name, None::<RemoveVolumeOptions>)
            .await
        {
            Ok(()) => Ok(()),
            // A volume that is already gone is the state the caller wanted.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(RuntimeError::api(Operation::RemoveVolume, e)),
        }
    }

    async fn list_images(&self, label: &str) -> Result<Vec<ImageSummary>, RuntimeError> {
        let options = ListImagesOptionsBuilder::new()
            .all(true)
            .filters(&HashMap::from([("label", vec![label])]))
            .build();
        let images = self
            .docker
            .list_images(Some(options))
            .await
            .map_err(|e| RuntimeError::api(Operation::ListImages, e))?;
        Ok(images
            .into_iter()
            .map(|image| ImageSummary {
                id: image.id,
                tags: image.repo_tags,
                size_bytes: u64::try_from(image.size).unwrap_or(0),
            })
            .collect())
    }

    async fn remove_image(&self, image: &str) -> Result<(), RuntimeError> {
        match self
            .docker
            .remove_image(image, None::<RemoveImageOptions>, None)
            .await
        {
            Ok(_) => Ok(()),
            // Already gone, same as `remove_volume`.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(RuntimeError::api(Operation::RemoveImage, e)),
        }
    }

    async fn stop_container(&self, id: &str) -> Result<(), RuntimeError> {
        let options = StopContainerOptionsBuilder::new()
            .t(STOP_GRACE_SECS)
            .build();
        self.docker
            .stop_container(id, Some(options))
            .await
            .map_err(|e| RuntimeError::api(Operation::StopContainer, e))
    }

    async fn remove_container(&self, id: &str) -> Result<(), RuntimeError> {
        // Force-remove so a still-running container is torn down cleanly on job
        // teardown rather than erroring.
        let options = RemoveContainerOptionsBuilder::new().force(true).build();
        self.docker
            .remove_container(id, Some(options))
            .await
            .map_err(|e| RuntimeError::api(Operation::RemoveContainer, e))
    }

    async fn exec(
        &self,
        container: &str,
        spec: &ExecSpec,
        sink: &mut (dyn ExecOutputSink + Send),
    ) -> Result<ExecOutput, RuntimeError> {
        let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let config = CreateExecOptions {
            cmd: Some(spec.cmd.clone()),
            env: Some(env),
            working_dir: spec.working_dir.clone(),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };
        let created = self
            .docker
            .create_exec(container, config)
            .await
            .map_err(|e| RuntimeError::api(Operation::CreateExec, e))?;
        match self
            .docker
            .start_exec(&created.id, None)
            .await
            .map_err(|e| RuntimeError::api(Operation::StartExec, e))?
        {
            StartExecResults::Attached { mut output, .. } => {
                while let Some(chunk) = output.next().await {
                    match chunk.map_err(|e| RuntimeError::api(Operation::StartExec, e))? {
                        LogOutput::StdOut { message } => sink.on_stdout(&message),
                        // Docker frames non-TTY stderr distinctly; a TTY exec
                        // collapses both onto `Console`, which we route to
                        // stdout as the terminal would.
                        LogOutput::StdErr { message } => sink.on_stderr(&message),
                        LogOutput::Console { message } => sink.on_stdout(&message),
                        LogOutput::StdIn { .. } => {}
                    }
                }
            }
            StartExecResults::Detached => {}
        }
        let inspect = self
            .docker
            .inspect_exec(&created.id)
            .await
            .map_err(|e| RuntimeError::api(Operation::InspectExec, e))?;
        // The stream has drained, so the exec has finished and the daemon
        // reports its code. A missing code would mean the daemon lost the exec;
        // surface it as a non-zero result rather than a false green.
        let exit_code = inspect.exit_code.unwrap_or(1);
        Ok(ExecOutput { exit_code })
    }

    async fn run_container(
        &self,
        id: &str,
        sink: &mut (dyn ExecOutputSink + Send),
    ) -> Result<ExecOutput, RuntimeError> {
        self.docker
            .start_container(id, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .map_err(|e| RuntimeError::api(Operation::StartContainer, e))?;

        let log_options = LogsOptionsBuilder::new()
            .follow(true)
            .stdout(true)
            .stderr(true)
            .build();
        let mut logs = self.docker.logs(id, Some(log_options));
        while let Some(item) = logs.next().await {
            match item.map_err(|e| RuntimeError::api(Operation::ContainerLogs, e))? {
                LogOutput::StdOut { message } => sink.on_stdout(&message),
                LogOutput::StdErr { message } => sink.on_stderr(&message),
                LogOutput::Console { message } => sink.on_stdout(&message),
                LogOutput::StdIn { .. } => {}
            }
        }

        let wait_options = WaitContainerOptionsBuilder::new().build();
        let mut wait_stream = self.docker.wait_container(id, Some(wait_options));
        let exit_code = match wait_stream.next().await {
            Some(Ok(response)) => response.status_code,
            // A container that already exited by the time `wait` was issued
            // can race the wait stream into reporting an empty/erroring
            // response instead of the real status -- the stream can miss an
            // already-exited container. Re-inspecting recovers the code the
            // daemon actually recorded; only fall back to the placeholder 1
            // if inspecting also fails or the daemon has no code for us.
            Some(Err(_)) | None => match self.inspect_container(id).await {
                Ok(state) => state.exit_code.unwrap_or(1),
                Err(_) => 1,
            },
        };
        Ok(ExecOutput { exit_code })
    }

    async fn export_path(&self, container: &str, path: &str) -> Result<Vec<u8>, RuntimeError> {
        let options = DownloadFromContainerOptionsBuilder::new()
            .path(path)
            .build();
        let mut stream = self
            .docker
            .download_from_container(container, Some(options));
        let mut archive = Vec::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| RuntimeError::api(Operation::ExportPath, e))?;
            archive.extend_from_slice(&bytes);
        }
        Ok(archive)
    }

    async fn create_network(&self, name: &str) -> Result<String, RuntimeError> {
        let request = NetworkCreateRequest {
            name: name.to_string(),
            driver: Some("bridge".to_string()),
            ..Default::default()
        };
        let response = self
            .docker
            .create_network(request)
            .await
            .map_err(|e| RuntimeError::api(Operation::CreateNetwork, e))?;
        Ok(response.id)
    }

    async fn inspect_network(&self, name: &str) -> Result<NetworkInfo, RuntimeError> {
        let network = self
            .docker
            .inspect_network(
                name,
                None::<bollard::query_parameters::InspectNetworkOptions>,
            )
            .await
            .map_err(|e| RuntimeError::api(Operation::InspectNetwork, e))?;
        // A bridge can carry both an IPv4 and an IPv6 config; Greenlit binds
        // v4 only, matching the v0 host support statement.
        let configs = network
            .ipam
            .and_then(|ipam| ipam.config)
            .unwrap_or_default();
        let ipv4 = configs
            .into_iter()
            .find(|config| config.gateway.as_ref().is_some_and(|g| !g.contains(':')));
        Ok(NetworkInfo {
            gateway: ipv4.as_ref().and_then(|config| config.gateway.clone()),
            subnet: ipv4.and_then(|config| config.subnet),
        })
    }

    async fn remove_network(&self, name: &str) -> Result<(), RuntimeError> {
        self.docker
            .remove_network(name)
            .await
            .map_err(|e| RuntimeError::api(Operation::RemoveNetwork, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote `tcp://` `DOCKER_HOST` is refused before any connection
    /// attempt, never silently binding the repository at the wrong path on
    /// another machine.
    #[test]
    fn connect_docker_host_rejects_a_remote_tcp_endpoint() {
        let err = connect_docker_host("tcp://10.0.0.2:2375").unwrap_err();
        assert!(matches!(err, RuntimeError::RemoteDockerHost { .. }));
        assert!(err.to_string().contains("remote daemon"));
    }

    #[test]
    fn connect_docker_host_rejects_ssh_transport() {
        let err = connect_docker_host("ssh://build-box").unwrap_err();
        assert!(matches!(err, RuntimeError::UnsupportedDockerHost { .. }));
    }

    #[test]
    fn connect_docker_host_accepts_local_endpoints() {
        // The invariant under test is the locality rule: none of these may be
        // classified remote/unsupported. Client construction itself may still
        // fail with `Connect` where the endpoint does not exist — bollard's
        // `connect_with_unix` touches the socket path, which is absent inside
        // an isolated workflow container (by invariant) — and that outcome is
        // equally acceptable here.
        for host in [
            "tcp://localhost:2375",
            "tcp://127.0.0.1:2375",
            "tcp://[::1]:2375",
            "unix:///var/run/docker.sock",
        ] {
            match connect_docker_host(host) {
                Ok(_) | Err(RuntimeError::Connect { .. }) => {}
                Err(other) => {
                    panic!("expected {host} to pass the locality rule, got {other:?}")
                }
            }
        }
    }

    fn layer_info(
        id: &str,
        status: &str,
        current: Option<i64>,
        total: Option<i64>,
    ) -> CreateImageInfo {
        CreateImageInfo {
            id: Some(id.to_string()),
            status: Some(status.to_string()),
            progress_detail: Some(bollard::models::ProgressDetail { current, total }),
            ..CreateImageInfo::default()
        }
    }

    #[test]
    fn pull_progress_sums_bytes_across_layers() {
        let mut layers = HashMap::new();
        fold_pull_progress(
            &mut layers,
            &layer_info("aa", "Downloading", Some(10), Some(100)),
        );
        let (current, total) = fold_pull_progress(
            &mut layers,
            &layer_info("bb", "Downloading", Some(5), Some(50)),
        );
        assert_eq!(current, 15);
        assert_eq!(total, Some(150));
    }

    #[test]
    fn pull_progress_total_is_unknown_until_every_layer_reports_one() {
        let mut layers = HashMap::new();
        fold_pull_progress(
            &mut layers,
            &layer_info("aa", "Downloading", Some(10), Some(100)),
        );
        // A layer announced with no byte counts yet.
        let (current, total) = fold_pull_progress(
            &mut layers,
            &CreateImageInfo {
                id: Some("bb".to_string()),
                status: Some("Pulling fs layer".to_string()),
                ..CreateImageInfo::default()
            },
        );
        assert_eq!(current, 10);
        assert_eq!(total, None, "an unknown layer total makes the sum unknown");
    }

    #[test]
    fn a_completed_layer_counts_its_full_size() {
        let mut layers = HashMap::new();
        fold_pull_progress(
            &mut layers,
            &layer_info("aa", "Downloading", Some(10), Some(100)),
        );
        // The daemon stops sending byte counts once a layer finishes; the
        // completion status pins the layer to its total.
        let (current, total) =
            fold_pull_progress(&mut layers, &layer_info("aa", "Pull complete", None, None));
        assert_eq!(current, 100);
        assert_eq!(total, Some(100));
    }

    #[test]
    fn an_untracked_info_line_leaves_the_tally_unchanged() {
        let mut layers = HashMap::new();
        fold_pull_progress(
            &mut layers,
            &layer_info("aa", "Downloading", Some(10), Some(100)),
        );
        // Pull-level status lines carry no layer id.
        let (current, total) = fold_pull_progress(
            &mut layers,
            &CreateImageInfo {
                status: Some("Digest: sha256:...".to_string()),
                ..CreateImageInfo::default()
            },
        );
        assert_eq!((current, total), (10, Some(100)));
    }
}
