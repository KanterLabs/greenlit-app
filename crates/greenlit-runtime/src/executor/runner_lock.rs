//! Materialize and identify every immutable runner profile before locking.

use std::collections::BTreeMap;

use greenlit_engine::planned::Evaluation;
use greenlit_engine::{ExecutionPlan, RunnerLockV1};
use greenlit_store::cas::CasStore;

use crate::ContainerEngine;
use crate::executor::ExecError;
use crate::progress::ProgressSink;
use crate::runner::{
    ContainerdStargzSnapshotter, EagerDockerSnapshotter, OciRunnerProvider, RunnerProvider,
    Snapshotter,
};

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

    let provider = OciRunnerProvider::new(content_store.clone(), offline);
    let snapshotter = EagerDockerSnapshotter::new(offline);
    let lazy_snapshotter = (!offline)
        .then(crate::runner::containerd::StargzConfig::from_environment)
        .flatten()
        .map(ContainerdStargzSnapshotter::new);
    let mut materialized = BTreeMap::new();
    for (image, _) in selected.values() {
        if materialized.contains_key(image.image_identifier()) {
            continue;
        }
        let profile = runner_profile::for_runner(*image);
        let manifest = provider.resolve(profile.image, profile.digest).await?;
        let prepared = if let Some(lazy) = &lazy_snapshotter {
            match lazy.prepare(engine, &manifest, progress).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    tracing::warn!(
                        target: "greenlit_runtime::runner",
                        %error,
                        "configured lazy provider is unavailable; using verified eager fallback"
                    );
                    snapshotter.prepare(engine, &manifest, progress).await?
                }
            }
        } else {
            snapshotter.prepare(engine, &manifest, progress).await?
        };
        let provider_identity = prepared.provider;
        let identity = prepared.identity;
        validate_platform(
            &manifest.pull_reference,
            &identity.os,
            &identity.architecture,
        )?;
        materialized.insert(
            image.image_identifier(),
            RunnerLockV1 {
                requested_label: String::new(),
                resolved_label: image.image_identifier().to_string(),
                provider: format!("github-arc/{};{provider_identity}", profile.image_os),
                image_reference: manifest.pull_reference,
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
