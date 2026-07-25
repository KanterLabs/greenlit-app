//! Backend-neutral runner resolution and snapshot preparation.
//!
//! A provider resolves a logical runner request to an immutable manifest. A
//! snapshotter then prepares that manifest for execution. Keeping the two
//! operations separate prevents static workflow analysis from becoming an
//! inferred "minimal runner": prefetch is advisory, while an unprefetched
//! object remains available through the selected snapshotter.

pub mod containerd;

use async_trait::async_trait;
use greenlit_store::cas::CasStore;
use greenlit_store::oci::RegistryResolver;

use crate::{ContainerEngine, ExecError, ImageIdentity, ProgressEvent, ProgressSink};

/// Immutable runner image selected before any job step executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerManifest {
    /// Authored OCI reference.
    pub requested_reference: String,
    /// Digest-qualified reference supplied to the execution backend.
    pub pull_reference: String,
    /// Verified OCI manifest digest.
    pub digest: String,
    /// Operating system from the logical runner contract.
    pub os: String,
    /// CPU architecture from the logical runner contract.
    pub architecture: String,
    /// Whether resolution reused verified local registry metadata.
    pub metadata_cache_hit: bool,
    /// Whether verified manifest annotations prove every layer is eStargz.
    pub lazy_compatible: bool,
}

/// Identity and materialization facts produced by a snapshotter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRunner {
    /// Identity reported by the execution backend after preparation.
    pub identity: ImageIdentity,
    /// Stable provider/snapshotter identity recorded in the RunLock.
    pub provider: String,
    /// Whether missing filesystem content can be fetched on first access.
    pub lazy: bool,
    /// Whether all logical runner bytes had arrived when preparation returned.
    pub fully_materialized: bool,
}

/// Resolves a logical runner reference without deciding how its filesystem is
/// materialized.
#[async_trait]
pub trait RunnerProvider: Send + Sync {
    /// Resolve `reference` to the exact expected digest and platform.
    async fn resolve(
        &self,
        reference: &str,
        expected_digest: &str,
    ) -> Result<RunnerManifest, ExecError>;

    /// Stable provider name for diagnostics.
    fn name(&self) -> &'static str;
}

/// Prepares a runner filesystem and creates clean writable job views from it.
#[async_trait]
pub trait Snapshotter: Send + Sync {
    /// Ensure `manifest` is ready to start, fetching verified bytes as needed.
    async fn prepare(
        &self,
        engine: &dyn ContainerEngine,
        manifest: &RunnerManifest,
        progress: &mut (dyn ProgressSink + Send),
    ) -> Result<PreparedRunner, ExecError>;

    /// Stable snapshotter name for diagnostics and environment fingerprints.
    fn name(&self) -> &'static str;
}

/// Registry-backed resolver shared by eager and lazy snapshotters.
#[derive(Clone)]
pub struct OciRunnerProvider {
    resolver: RegistryResolver,
    offline: bool,
}

impl OciRunnerProvider {
    /// Creates a resolver backed by the machine-wide verified CAS.
    #[must_use]
    pub fn new(content_store: CasStore, offline: bool) -> Self {
        Self {
            resolver: RegistryResolver::new(content_store),
            offline,
        }
    }
}

#[async_trait]
impl RunnerProvider for OciRunnerProvider {
    async fn resolve(
        &self,
        reference: &str,
        expected_digest: &str,
    ) -> Result<RunnerManifest, ExecError> {
        let resolver = self.resolver.clone();
        let reference_for_task = reference.to_string();
        let offline = self.offline;
        let resolved = tokio::task::spawn_blocking(move || {
            if offline {
                resolver.resolve_linux_amd64_offline(&reference_for_task)
            } else {
                resolver.resolve_linux_amd64(&reference_for_task)
            }
        })
        .await
        .map_err(|error| ExecError::Infrastructure {
            message: format!("runner resolution task for '{reference}' did not complete: {error}"),
            fix: "retry; if this repeats, preserve the run directory and file a Greenlit defect"
                .to_string(),
        })?
        .map_err(|error| ExecError::Infrastructure {
            message: format!("could not resolve runner profile '{reference}': {error}"),
            fix: if offline {
                "run once without `--offline` to fetch and verify this exact runner profile"
                    .to_string()
            } else {
                "check registry connectivity, then retry".to_string()
            },
        })?;
        if resolved.digest.as_str() != expected_digest {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "runner profile '{reference}' resolved as {}, but its locked identity is {expected_digest}",
                    resolved.digest
                ),
                fix: "preserve the run directory and file a Greenlit defect".to_string(),
            });
        }
        Ok(RunnerManifest {
            requested_reference: reference.to_string(),
            pull_reference: resolved.pull_reference,
            digest: resolved.digest.to_string(),
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
            metadata_cache_hit: resolved.cache_hit,
            lazy_compatible: resolved.lazy_compatible,
        })
    }

    fn name(&self) -> &'static str {
        "oci-registry"
    }
}

/// Universal verified fallback: Docker eagerly materializes the entire image.
#[derive(Debug, Clone, Copy)]
pub struct EagerDockerSnapshotter {
    offline: bool,
}

/// Direct containerd remote-snapshotter preparation for configured hosts.
#[derive(Debug, Clone)]
pub struct ContainerdStargzSnapshotter {
    config: containerd::StargzConfig,
    access_profile: Vec<String>,
}

impl ContainerdStargzSnapshotter {
    /// Creates a lazy snapshotter with advisory paths to prefetch. Missing
    /// paths remain demand-fetched by stargz and are never treated as absent.
    #[must_use]
    pub fn new(config: containerd::StargzConfig, access_profile: Vec<String>) -> Self {
        Self {
            config,
            access_profile,
        }
    }
}

#[async_trait]
impl Snapshotter for ContainerdStargzSnapshotter {
    async fn prepare(
        &self,
        engine: &dyn ContainerEngine,
        manifest: &RunnerManifest,
        progress: &mut (dyn ProgressSink + Send),
    ) -> Result<PreparedRunner, ExecError> {
        progress.on_progress(ProgressEvent::ContentResolved {
            item: format!("runner {} (lazy stargz)", manifest.requested_reference),
            identity: manifest.digest.clone(),
            cache_hit: manifest.metadata_cache_hit,
        });
        if !manifest.lazy_compatible {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "runner '{}' is not an eStargz image with verified per-layer TOC identities",
                    manifest.pull_reference
                ),
                fix: "use the verified eager fallback or select a runner manifest published in eStargz OCI form"
                    .to_string(),
            });
        }
        let client = containerd::StargzClient::connect(self.config.clone())
            .await
            .map_err(stargz_error)?;
        client
            .prepare(&manifest.pull_reference, &self.access_profile)
            .await
            .map_err(stargz_error)?;
        let identity = engine
            .image_identity(&manifest.pull_reference)
            .await?
            .ok_or_else(|| ExecError::Infrastructure {
                message: format!(
                    "containerd prepared '{}' but the configured execution runtime cannot address that snapshot",
                    manifest.pull_reference
                ),
                fix: "configure GREENLIT_CONTAINERD_NAMESPACE to the execution runtime's shared namespace, or use the eager Docker fallback"
                    .to_string(),
            })?;
        if identity.digest != manifest.digest {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "lazy snapshot identity {} does not match the locked runner {}",
                    identity.digest, manifest.digest
                ),
                fix: "remove the conflicting containerd image and retry".to_string(),
            });
        }
        Ok(PreparedRunner {
            identity,
            provider: format!("containerd-{}-lazy", self.config.snapshotter),
            lazy: true,
            fully_materialized: false,
        })
    }

    fn name(&self) -> &'static str {
        "containerd-stargz"
    }
}

fn stargz_error(error: containerd::StargzError) -> ExecError {
    ExecError::Infrastructure {
        message: format!("lazy runner preparation failed: {error}"),
        fix: "repair the configured containerd/stargz provider or remove its GREENLIT_CONTAINERD_* configuration to use verified eager Docker"
            .to_string(),
    }
}

impl EagerDockerSnapshotter {
    /// Creates the fallback snapshotter.
    #[must_use]
    pub fn new(offline: bool) -> Self {
        Self { offline }
    }
}

#[async_trait]
impl Snapshotter for EagerDockerSnapshotter {
    async fn prepare(
        &self,
        engine: &dyn ContainerEngine,
        manifest: &RunnerManifest,
        progress: &mut (dyn ProgressSink + Send),
    ) -> Result<PreparedRunner, ExecError> {
        progress.on_progress(ProgressEvent::ContentResolved {
            item: format!("runner {}", manifest.requested_reference),
            identity: manifest.digest.clone(),
            cache_hit: manifest.metadata_cache_hit,
        });
        if self.offline {
            if !engine.image_exists(&manifest.pull_reference).await? {
                return Err(ExecError::Infrastructure {
                    message: format!(
                        "offline content is missing: runner profile {}",
                        manifest.pull_reference
                    ),
                    fix: "run once without `--offline` to fetch this exact runner profile"
                        .to_string(),
                });
            }
        } else {
            engine
                .pull_image(&manifest.pull_reference, None, progress)
                .await?;
        }
        let identity = engine
            .image_identity(&manifest.pull_reference)
            .await?
            .ok_or_else(|| ExecError::Infrastructure {
                message: format!(
                    "runner profile '{}' has no immutable identity after materialization",
                    manifest.pull_reference
                ),
                fix: "run `litci doctor`; if inspection remains unavailable, use the supported Docker backend"
                    .to_string(),
            })?;
        if identity.digest != manifest.digest {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "container engine materialized runner profile '{}' as {}, but the lock requires {}",
                    manifest.pull_reference, identity.digest, manifest.digest
                ),
                fix: "remove the conflicting local image and retry the exact locked digest"
                    .to_string(),
            });
        }
        Ok(PreparedRunner {
            identity,
            provider: "oci-registry+docker-eager".to_string(),
            lazy: false,
            fully_materialized: true,
        })
    }

    fn name(&self) -> &'static str {
        "docker-eager"
    }
}
