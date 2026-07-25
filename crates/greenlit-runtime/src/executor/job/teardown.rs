//! Winding a job instance down after its outcome is known: committing a
//! converged image for a green run on a Greenlit runner image, then
//! releasing the container, its Docker-sibling volumes, the DinD sidecar,
//! started services, and finally the job's own network.

use indexmap::IndexMap;

use greenlit_engine::Conclusion;

use crate::executor::actions::docker_action;
use crate::executor::container::namespaced_volume_name;
use crate::executor::dind;
use crate::executor::provision;
use crate::executor::report::StepReport;
use crate::executor::services;
use crate::executor::{ExecError, Shared};

/// Commit the job's container into a converged image when this run is
/// eligible.
///
/// Convergence happens only for a green job on a Greenlit runner image:
/// committing a container whose steps failed would bake a half-installed
/// state into the image every later run starts from.
// The outcome type mirrors `run_job_body`'s return type verbatim; boxing or
// aliasing it would be a bigger footprint than one lint allow.
#[allow(clippy::type_complexity)]
pub(super) async fn converge(
    shared: &Shared<'_>,
    container: &str,
    in_container: bool,
    outcome: &Result<(Vec<StepReport>, IndexMap<String, String>, Conclusion), ExecError>,
    converged_target: &Option<String>,
    converged_source: &Option<(provision::manifest::Manifest, String)>,
) {
    if !in_container
        && matches!(outcome, Ok((_, _, Conclusion::Success)))
        && let Some(tag) = converged_target
        && let Some((manifest, base_image)) = converged_source
    {
        let installed = provision::provisioned_commands(shared.engine, container).await;
        provision::build_converged(shared.engine, base_image, manifest, &installed, tag).await;
    }
}

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
) {
    if !shared.config.write_back {
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
                let volume = namespaced_volume_name(&shared.config.volume_namespace, source);
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
