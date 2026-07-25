//! Pre-execution OCI alias resolution for RunLock finalization.

use std::collections::{BTreeMap, BTreeSet};

use greenlit_engine::planned::Evaluation;
use greenlit_engine::{ContainerPlan, ExecutionPlan, StepKind};
use greenlit_store::cas::CasStore;
use greenlit_store::oci::RegistryResolver;

use crate::{ContainerEngine, ProgressSink};

use super::ExecError;

/// Pulls every statically selected job/service/bare-Docker-action image,
/// inspects its immutable digest and platform, and rejects mismatched
/// architectures before a workflow step can execute.
///
/// # Errors
/// Returns an engine error for pull/inspect failure and an infrastructure
/// error when an image has no immutable identity or is not Linux amd64.
pub async fn preflight_plan_images(
    engine: &dyn ContainerEngine,
    plan: &ExecutionPlan,
    additional_references: &[String],
    content_store: &CasStore,
    offline: bool,
    progress: &mut (dyn ProgressSink + Send),
) -> Result<BTreeMap<String, String>, ExecError> {
    let mut references = BTreeSet::new();
    references.insert(crate::executor::netguard::NETGUARD_IMAGE.to_string());
    for job in &plan.jobs {
        collect_container(job.container.as_ref(), &mut references)?;
        for service in job.services.values() {
            collect_container(Some(service), &mut references)?;
        }
        collect_docker_steps(&job.steps, &mut references);
        for leg in &job.legs {
            collect_container(leg.container.as_ref(), &mut references)?;
            for service in leg.services.values() {
                collect_container(Some(service), &mut references)?;
            }
            collect_docker_steps(&leg.steps, &mut references);
        }
    }
    references.extend(additional_references.iter().cloned());
    let mut identities = BTreeMap::new();
    let resolver = RegistryResolver::new(content_store.clone());
    for reference in references {
        let resolver = resolver.clone();
        let reference_for_task = reference.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            if offline {
                resolver.resolve_linux_amd64_offline(&reference_for_task)
            } else {
                resolver.resolve_linux_amd64(&reference_for_task)
            }
        })
        .await
        .map_err(|error| ExecError::Infrastructure {
            message: format!(
                "container image resolution task for '{reference}' did not complete: {error}"
            ),
            fix: "retry; if this repeats, preserve the run directory and file a Greenlit defect"
                .to_string(),
        })?
        .map_err(|error| ExecError::Infrastructure {
            message: format!("could not resolve container image '{reference}': {error}"),
            fix: if offline {
                "run once without `--offline` to fetch and verify this exact image".to_string()
            } else {
                "check registry connectivity and credentials, then retry".to_string()
            },
        })?;
        progress.on_progress(crate::ProgressEvent::ContentResolved {
            item: reference.clone(),
            identity: resolved.digest.to_string(),
            cache_hit: resolved.cache_hit,
        });
        if offline {
            if !engine.image_exists(&resolved.pull_reference).await? {
                return Err(ExecError::Infrastructure {
                    message: format!(
                        "offline content is missing: container image {}",
                        resolved.pull_reference
                    ),
                    fix: "run once without `--offline` to fetch this exact locked image"
                        .to_string(),
                });
            }
        } else {
            engine
                .pull_image(&resolved.pull_reference, None, progress)
                .await?;
        }
        let identity = engine
            .image_identity(&resolved.pull_reference)
            .await?
            .ok_or_else(|| ExecError::Infrastructure {
                message: format!(
                    "container image '{}' has no immutable identity after materialization",
                    resolved.pull_reference
                ),
                fix: "use a registry image that exposes an OCI digest".to_string(),
            })?;
        if identity.digest != resolved.digest.as_str() {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "container engine materialized '{}' as {}, but the locked registry manifest is {}",
                    resolved.pull_reference, identity.digest, resolved.digest
                ),
                fix: "remove the conflicting local image and retry the exact locked digest"
                    .to_string(),
            });
        }
        if identity.os != "linux"
            || (identity.architecture != "amd64" && identity.architecture != "x86_64")
        {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "container image '{reference}' resolved to unsupported platform {}/{}",
                    identity.os, identity.architecture
                ),
                fix: "select a Linux amd64 image or run on a matching supported host".to_string(),
            });
        }
        identities.insert(reference, resolved.digest.to_string());
    }
    Ok(identities)
}

fn collect_container(
    container: Option<&ContainerPlan>,
    references: &mut BTreeSet<String>,
) -> Result<(), ExecError> {
    let Some(container) = container else {
        return Ok(());
    };
    match &container.image.evaluation {
        Evaluation::Static(reference) => {
            references.insert(reference.clone());
            Ok(())
        }
        Evaluation::Deferred(_) => Err(ExecError::Infrastructure {
            message: "a container image remains runtime-dependent at RunLock finalization"
                .to_string(),
            fix: "select a concrete matrix case or replace the image expression with a statically resolvable value"
                .to_string(),
        }),
    }
}

fn collect_docker_steps(steps: &[greenlit_engine::StepPlan], references: &mut BTreeSet<String>) {
    for step in steps {
        if let StepKind::Uses { reference, .. } = &step.kind
            && let Some(image) = reference.strip_prefix("docker://")
        {
            references.insert(image.to_string());
        }
    }
}
