//! Winding a job instance down: release the container, Docker-sibling
//! volumes, DinD sidecar, services, and finally the job's own network.

use crate::executor::Shared;
use crate::executor::actions::docker_action;
use crate::executor::container::namespaced_volume_name;
use crate::executor::dind;
use crate::executor::services;

/// `--write-back` needs the container (and its overlay upper) reachable
/// after the run to export the diff (`PHASE-2-execution.md` "Overlay
/// isolation": "export the upper-layer diff ... after the run"); the caller
/// (`litci run`) removes it once write-back has processed this job.
/// Otherwise, best-effort teardown here and now: a leaked container is not a
/// run failure, and it must not mask the job's real result or error.
pub(super) async fn teardown(
    shared: &Shared<'_>,
    container: &str,
    docker_volumes: Option<docker_action::SiblingVolumes>,
    dind: Option<&dind::Dind>,
    services_started: &[services::StartedService],
    job_network: services::JobNetwork,
    keep_container: bool,
) {
    if !keep_container {
        let _ = shared.engine.remove_container(container).await;
        // The Docker-sibling volumes outlive the container that bound them,
        // so removing the container is not enough. Before `remove_volume`
        // existed on the port these accumulated on the host until an
        // operator ran `docker volume prune` -- the module doc in
        // `actions::docker_action` already described a per-job removal that
        // no code performed. Removal must follow the container, because a
        // volume still in use cannot be removed.
        if let Some(volumes) = docker_volumes {
            for source in [volumes.workspace, volumes.cmdfiles] {
                let volume = namespaced_volume_name(shared.namespace, source);
                let _ = shared.engine.remove_volume(&volume).await;
            }
        }
    }
    // After the container, before the network: services and the DinD sidecar
    // hold attachments too, and a network with any attachment cannot be
    // removed.
    if let Some(sidecar) = dind {
        let _ = shared.engine.remove_container(sidecar.container()).await;
    }
    services::stop(shared.engine, services_started).await;
    job_network.teardown(shared.engine).await;
}
