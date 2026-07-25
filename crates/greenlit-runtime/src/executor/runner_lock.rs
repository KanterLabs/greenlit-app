//! Materialize and identify every runner environment before RunLock finalization.

use std::collections::BTreeMap;

use greenlit_engine::planned::Evaluation;
use greenlit_engine::{ExecutionPlan, RunnerImage, RunnerLockV1};

use crate::ContainerEngine;
use crate::executor::ExecError;
use crate::image::ensure_base_image;
use crate::platform::UbuntuRelease;
use crate::progress::ProgressSink;

/// Ensures every statically selected runner image exists and returns its
/// immutable identity by concrete job key.
///
/// # Errors
///
/// Fails closed when a runner remains runtime-dependent, cannot be
/// materialized, lacks an inspectable identity, or has the wrong platform.
pub async fn preflight_plan_runners(
    engine: &dyn ContainerEngine,
    plan: &ExecutionPlan,
    progress: &mut (dyn ProgressSink + Send),
) -> Result<BTreeMap<String, RunnerLockV1>, ExecError> {
    let mut selected = BTreeMap::new();
    for job in &plan.jobs {
        if let Some(runner) = &job.runner {
            let Evaluation::Static(image) = runner.evaluation else {
                return Err(runtime_dependent_runner(&job.id.0));
            };
            selected.insert(job.id.0.clone(), (image, runner.source.clone()));
        }
        for (index, leg) in job.legs.iter().enumerate() {
            let Evaluation::Static(image) = leg.runner.evaluation else {
                return Err(runtime_dependent_runner(&format!("{}[{index}]", job.id.0)));
            };
            selected.insert(
                format!("{}[{index}]", job.id.0),
                (image, leg.runner.source.clone()),
            );
        }
    }

    let mut materialized = BTreeMap::new();
    for (image, _) in selected.values() {
        if materialized.contains_key(image.image_identifier()) {
            continue;
        }
        let reference = ensure_base_image(engine, release_for(*image), progress).await?;
        let identity = engine.image_identity(&reference).await?.ok_or_else(|| {
            ExecError::Infrastructure {
                message: format!(
                    "runner image '{reference}' has no immutable identity after materialization"
                ),
                fix: "run `litci doctor`; if inspection remains unavailable, use the supported Docker backend"
                    .to_string(),
            }
        })?;
        validate_platform(&reference, &identity.os, &identity.architecture)?;
        materialized.insert(
            image.image_identifier(),
            RunnerLockV1 {
                requested_label: String::new(),
                resolved_label: image.image_identifier().to_string(),
                provider: "greenlit-base".to_string(),
                image_reference: reference,
                image_digest: identity.digest,
                os: identity.os,
                architecture: identity.architecture,
                runner_version: format!("greenlit-runtime/{}", env!("CARGO_PKG_VERSION")),
            },
        );
    }

    selected
        .into_iter()
        .map(|(key, (image, requested_label))| {
            materialized
                .get(image.image_identifier())
                .cloned()
                .map(|mut identity| {
                    identity.requested_label = requested_label;
                    (key, identity)
                })
                .ok_or_else(|| ExecError::Infrastructure {
                    message: format!(
                        "runner environment '{}' was not materialized",
                        image.image_identifier()
                    ),
                    fix: "preserve the run directory and file a Greenlit defect".to_string(),
                })
        })
        .collect()
}

fn release_for(image: RunnerImage) -> UbuntuRelease {
    match image {
        RunnerImage::Ubuntu2404 => UbuntuRelease::Noble2404,
        RunnerImage::Ubuntu2204 => UbuntuRelease::Jammy2204,
    }
}

fn validate_platform(reference: &str, os: &str, architecture: &str) -> Result<(), ExecError> {
    if os == "linux" && (architecture == "amd64" || architecture == "x86_64") {
        return Ok(());
    }
    Err(ExecError::Infrastructure {
        message: format!(
            "runner image '{reference}' resolved to unsupported platform {os}/{architecture}"
        ),
        fix: "run on a Linux x86_64 host with a native Linux amd64 runner image".to_string(),
    })
}

fn runtime_dependent_runner(job: &str) -> ExecError {
    ExecError::Infrastructure {
        message: format!("runner for '{job}' remains runtime-dependent at RunLock finalization"),
        fix: "select a concrete matrix case or use a statically resolvable runs-on label"
            .to_string(),
    }
}
