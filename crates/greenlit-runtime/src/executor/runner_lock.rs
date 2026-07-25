//! Materialize and identify every immutable runner profile before locking.

use std::collections::BTreeMap;

use greenlit_engine::planned::Evaluation;
use greenlit_engine::{ExecutionPlan, RunnerLockV1};
use greenlit_store::cas::CasStore;
use greenlit_store::oci::RegistryResolver;

use crate::ContainerEngine;
use crate::executor::ExecError;
use crate::progress::ProgressSink;

use super::runner_profile;

/// Ensures every statically selected runner profile exists and returns its
/// immutable identity by concrete job key.
///
/// # Errors
///
/// Fails closed when a runner remains runtime-dependent, verified registry
/// metadata is absent offline, a profile cannot be materialized, or the
/// engine reports an identity/platform different from the profile lock.
pub async fn preflight_plan_runners(
    engine: &dyn ContainerEngine,
    plan: &ExecutionPlan,
    content_store: &CasStore,
    offline: bool,
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

    let resolver = RegistryResolver::new(content_store.clone());
    let mut materialized = BTreeMap::new();
    for (image, _) in selected.values() {
        if materialized.contains_key(image.image_identifier()) {
            continue;
        }
        let profile = runner_profile::for_runner(*image);
        let resolver = resolver.clone();
        let reference_for_task = profile.image.to_string();
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
                "runner profile resolution task for '{}' did not complete: {error}",
                profile.image
            ),
            fix: "retry; if this repeats, preserve the run directory and file a Greenlit defect"
                .to_string(),
        })?
        .map_err(|error| ExecError::Infrastructure {
            message: format!(
                "could not resolve runner profile '{}': {error}",
                profile.image
            ),
            fix: if offline {
                "run once without `--offline` to fetch and verify this exact runner profile"
                    .to_string()
            } else {
                "check registry connectivity, then retry".to_string()
            },
        })?;
        if resolved.digest.as_str() != profile.digest {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "runner profile '{}' resolved as {}, but its locked identity is {}",
                    profile.image, resolved.digest, profile.digest
                ),
                fix: "preserve the run directory and file a Greenlit defect".to_string(),
            });
        }
        progress.on_progress(crate::ProgressEvent::ContentResolved {
            item: format!("runner {}", image.image_identifier()),
            identity: profile.digest.to_string(),
            cache_hit: resolved.cache_hit,
        });
        if offline {
            if !engine.image_exists(&resolved.pull_reference).await? {
                return Err(ExecError::Infrastructure {
                    message: format!(
                        "offline content is missing: runner profile {}",
                        resolved.pull_reference
                    ),
                    fix: "run once without `--offline` to fetch this exact runner profile"
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
                    "runner profile '{}' has no immutable identity after materialization",
                    resolved.pull_reference
                ),
                fix: "run `litci doctor`; if inspection remains unavailable, use the supported Docker backend"
                    .to_string(),
            })?;
        if identity.digest != profile.digest {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "container engine materialized runner profile '{}' as {}, but the lock requires {}",
                    resolved.pull_reference, identity.digest, profile.digest
                ),
                fix: "remove the conflicting local image and retry the exact locked digest"
                    .to_string(),
            });
        }
        validate_platform(
            &resolved.pull_reference,
            &identity.os,
            &identity.architecture,
        )?;
        materialized.insert(
            image.image_identifier(),
            RunnerLockV1 {
                requested_label: String::new(),
                resolved_label: image.image_identifier().to_string(),
                provider: format!("github-arc/{}", profile.image_os),
                image_reference: resolved.pull_reference,
                image_digest: identity.digest,
                os: identity.os,
                architecture: identity.architecture,
                runner_version: profile.runner_version.to_string(),
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

fn validate_platform(reference: &str, os: &str, architecture: &str) -> Result<(), ExecError> {
    if os == "linux" && (architecture == "amd64" || architecture == "x86_64") {
        return Ok(());
    }
    Err(ExecError::Infrastructure {
        message: format!(
            "runner profile '{reference}' resolved to unsupported platform {os}/{architecture}"
        ),
        fix: "run on a Linux x86_64 host with a native Linux amd64 runner profile".to_string(),
    })
}

fn runtime_dependent_runner(job: &str) -> ExecError {
    ExecError::Infrastructure {
        message: format!("runner for '{job}' remains runtime-dependent at RunLock finalization"),
        fix: "select a concrete matrix case or use a statically resolvable runs-on label"
            .to_string(),
    }
}
