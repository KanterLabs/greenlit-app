//! The convergent base image: assembly, content-addressed tagging, and
//! build-on-first-use through the [`ContainerEngine`] port.
//!
//! `PHASE-2-execution.md` ("Base image and private init helper"): a single
//! Dockerfile for every supported Ubuntu label (`bash`, `git`, `curl`, `wget`,
//! `jq`, `tar`, `unzip`, `build-essential`, `ca-certificates`, plus the private
//! `greenlit-init` helper), built through the engine API on first use and
//! tagged with the label and a content hash. The helper's bytes are embedded at
//! build time by this crate's build script and baked into the build context
//! here — extracted only into the context, never installed as a host command.
//!
//! [`ContainerEngine`]: crate::ContainerEngine

mod context;

use crate::engine::{BuildSpec, ContainerEngine};
use crate::error::RuntimeError;
use crate::platform::UbuntuRelease;
use crate::progress::ProgressSink;

/// The base-image repository (`greenlit/*` namespace, `AGENTS.md`).
pub const IMAGE_REPO: &str = "greenlit/base";

/// Absolute in-image path of the embedded helper — the container entrypoint.
/// Placed under `/greenlit/bin` so it never shadows a repo tool on `PATH`.
pub const INIT_IN_IMAGE_PATH: &str = "/greenlit/bin/greenlit-init";

/// The Dockerfile, embedded from this crate's packaged
/// `images/base/Dockerfile`.
///
/// It must live in the binary because `litci` is one distributable host binary
/// that assembles the build context in memory — there is no companion file to
/// read at runtime.
const DOCKERFILE: &str = include_str!("../../images/base/Dockerfile");

/// The embedded `greenlit-init` binary, staged into `OUT_DIR` by `build.rs`.
const INIT_BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/greenlit-init"));

/// The private helper's embedded bytes (the container entrypoint binary).
///
/// Exposed so integration tests can bake the exact same helper into a probe
/// image without rebuilding it.
#[must_use]
pub fn init_binary() -> &'static [u8] {
    INIT_BINARY
}

/// A resolved plan to build one base image: its content-addressed tag and the
/// [`BuildSpec`] the engine ships to the daemon.
#[derive(Debug, Clone)]
pub struct BaseImagePlan {
    /// The full `greenlit/base:ubuntu-<version>-<hash>` reference.
    pub tag: String,
    /// The build request (context tar, `BASE_IMAGE` arg, tag).
    pub build_spec: BuildSpec,
}

/// Anything that can go wrong assembling or ensuring the base image.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    /// The in-memory build-context tar could not be assembled.
    #[error("failed to assemble the base-image build context: {0}")]
    Context(#[source] std::io::Error),

    /// A Docker Engine API call (image existence check or build) failed.
    #[error(transparent)]
    Engine(#[from] RuntimeError),
}

/// Compute the build plan for `release` using the embedded helper bytes.
///
/// The tag is `greenlit/base:ubuntu-<version>-<hash>` where `<hash>` covers the
/// Dockerfile, the upstream base reference, and the helper bytes, so identical
/// inputs converge on one tag and any change forces a rebuild.
///
/// # Errors
///
/// Returns [`ImageError::Context`] if the build-context tar cannot be built.
pub fn plan_base_image(release: UbuntuRelease) -> Result<BaseImagePlan, ImageError> {
    plan_with_helper(release, INIT_BINARY)
}

/// [`plan_base_image`] parameterised on the helper bytes, so tests can pin the
/// tag scheme without depending on the embedded binary's exact contents.
fn plan_with_helper(
    release: UbuntuRelease,
    init_bytes: &[u8],
) -> Result<BaseImagePlan, ImageError> {
    let base_ref = release.base_image_ref();
    let hash = context::content_hash(DOCKERFILE, base_ref, init_bytes);
    let tag = format!("{IMAGE_REPO}:ubuntu-{}-{hash}", release.version());
    let context_tar =
        context::build_context_tar(DOCKERFILE, init_bytes).map_err(ImageError::Context)?;
    let build_spec = BuildSpec {
        context_tar,
        dockerfile: context::DOCKERFILE_NAME.to_string(),
        tag: tag.clone(),
        build_args: vec![("BASE_IMAGE".to_string(), base_ref.to_string())],
    };
    Ok(BaseImagePlan { tag, build_spec })
}

/// Ensure the base image for `release` exists, building it on first use.
///
/// Returns the image tag. If a content-hash-identical image is already present
/// the daemon build is skipped (`PHASE-2-execution.md`: "Build through the
/// engine API on first use").
///
/// # Errors
///
/// Returns [`ImageError::Context`] if the context cannot be assembled, or
/// [`ImageError::Engine`] if the existence check or the build fails.
pub async fn ensure_base_image(
    engine: &dyn ContainerEngine,
    release: UbuntuRelease,
    progress: &mut (dyn ProgressSink + Send),
) -> Result<String, ImageError> {
    let plan = plan_base_image(release)?;
    if engine.image_exists(&plan.tag).await? {
        return Ok(plan.tag);
    }
    engine.build_image(&plan.build_spec, progress).await?;
    Ok(plan.tag)
}
