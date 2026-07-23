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
use bollard::models::{ContainerConfig, ContainerCreateBody, HostConfig, NetworkCreateRequest};
use bollard::query_parameters::{
    BuildImageOptionsBuilder, CommitContainerOptionsBuilder, CreateContainerOptionsBuilder,
    CreateImageOptionsBuilder, DownloadFromContainerOptionsBuilder, RemoveContainerOptionsBuilder,
    StopContainerOptionsBuilder,
};
use bytes::Bytes;
use futures_util::StreamExt;

use crate::detect::{DockerHostRejection, Endpoint, reject_docker_host};
use crate::engine::{
    BuildSpec, CommitSpec, ContainerEngine, ContainerSpec, ExecOutput, ExecOutputSink, ExecSpec,
};
use crate::error::{Operation, RuntimeError};

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

/// Splits an image reference into `(name, tag)`.
///
/// Docker's `/images/create` endpoint takes `fromImage` and `tag` as separate
/// query parameters. The tag is the segment after the final `:` unless that
/// segment contains a `/` (which means the `:` was a registry-host port, e.g.
/// `localhost:5000/img`). A reference with no tag defaults to `latest`.
fn split_reference(image: &str) -> (&str, &str) {
    match image.rfind(':') {
        Some(idx) if !image[idx + 1..].contains('/') => (&image[..idx], &image[idx + 1..]),
        _ => (image, "latest"),
    }
}

#[async_trait]
impl ContainerEngine for DockerEngine {
    async fn pull_image(&self, image: &str) -> Result<(), RuntimeError> {
        let (name, tag) = split_reference(image);
        let options = CreateImageOptionsBuilder::new()
            .from_image(name)
            .tag(tag)
            .build();
        let mut stream = self.docker.create_image(Some(options), None, None);
        while let Some(item) = stream.next().await {
            item.map_err(|e| RuntimeError::api(Operation::PullImage, e))?;
        }
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

    async fn build_image(&self, spec: &BuildSpec) -> Result<(), RuntimeError> {
        let build_args: HashMap<String, String> = spec.build_args.iter().cloned().collect();
        let options = BuildImageOptionsBuilder::new()
            .dockerfile(&spec.dockerfile)
            .t(&spec.tag)
            .buildargs(&build_args)
            .rm(true)
            .build();
        let body = bollard::body_full(Bytes::from(spec.context_tar.clone()));
        let mut stream = self.docker.build_image(options, None, Some(body));
        while let Some(item) = stream.next().await {
            item.map_err(|e| RuntimeError::api(Operation::BuildImage, e))?;
        }
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
        let host_config = (spec.network.is_some() || !binds.is_empty()).then(|| HostConfig {
            network_mode: spec.network.clone(),
            binds: (!binds.is_empty()).then_some(binds),
            ..Default::default()
        });
        let body = ContainerCreateBody {
            image: Some(spec.image.clone()),
            entrypoint: (!spec.entrypoint.is_empty()).then(|| spec.entrypoint.clone()),
            cmd: (!spec.cmd.is_empty()).then(|| spec.cmd.clone()),
            env: (!env.is_empty()).then_some(env),
            working_dir: spec.working_dir.clone(),
            labels: (!labels.is_empty()).then_some(labels),
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
        // Constructing a bollard HTTP/unix client is pure local setup — no
        // network round-trip — so these succeed even without a live daemon.
        for host in [
            "tcp://localhost:2375",
            "tcp://127.0.0.1:2375",
            "tcp://[::1]:2375",
            "unix:///var/run/docker.sock",
        ] {
            assert!(
                connect_docker_host(host).is_ok(),
                "expected {host} to be accepted"
            );
        }
    }
}
