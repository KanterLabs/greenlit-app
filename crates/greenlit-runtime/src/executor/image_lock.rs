//! Pre-execution OCI alias resolution for RunLock finalization.

use std::collections::{BTreeMap, BTreeSet};

use greenlit_engine::planned::Evaluation;
use greenlit_engine::{ContainerPlan, ExecutionPlan, StepKind};

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
    for reference in references {
        engine.pull_image(&reference, None, progress).await?;
        let identity = engine.image_identity(&reference).await?.ok_or_else(|| {
            ExecError::Infrastructure {
                message: format!(
                    "container image '{reference}' has no immutable identity after materialization"
                ),
                fix: "use a registry image that exposes an OCI digest".to_string(),
            }
        })?;
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
        identities.insert(reference, identity.digest);
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
