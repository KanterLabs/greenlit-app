//! Immutable Greenlit runner profiles.
//!
//! These are GitHub's official Actions Runner Controller images
//! (<https://docs.github.com/en/actions/concepts/runners/actions-runner-controller#software-installed-in-the-arc-runner-image>),
//! pinned to
//! Linux amd64 platform-manifest digests. They are deliberately identified as
//! self-hosted runner profiles rather than hosted-runner images: the support
//! report records that distinction, while the immutable identity prevents a
//! later tag move or package-repository change from altering a run.

use greenlit_engine::RunnerImage;

/// One immutable Linux amd64 runner profile.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunnerProfile {
    /// Canonical OCI reference used for materialization and execution.
    pub(crate) image: &'static str,
    /// Exact platform manifest digest.
    pub(crate) digest: &'static str,
    /// Embedded GitHub Actions runner version.
    pub(crate) runner_version: &'static str,
    /// OS identity published by the image.
    pub(crate) image_os: &'static str,
}

const UBUNTU_2404: RunnerProfile = RunnerProfile {
    image: "ghcr.io/actions/actions-runner@sha256:a1919047b038c38871d667c58cfdc7a878452711ab1212fb6036188f27a7ab16",
    digest: "sha256:a1919047b038c38871d667c58cfdc7a878452711ab1212fb6036188f27a7ab16",
    runner_version: "2.336.0",
    image_os: "ubuntu24",
};

const UBUNTU_2204: RunnerProfile = RunnerProfile {
    image: "ghcr.io/actions/actions-runner@sha256:7cde2ec035c9f4cc965f702f434ef6ca39ab027fff7fdab8cc738e933ba392fb",
    digest: "sha256:7cde2ec035c9f4cc965f702f434ef6ca39ab027fff7fdab8cc738e933ba392fb",
    runner_version: "2.321.0",
    image_os: "ubuntu22",
};

/// Returns the immutable profile selected for a planned runner.
#[must_use]
pub(crate) fn for_runner(runner: RunnerImage) -> RunnerProfile {
    match runner {
        RunnerImage::Ubuntu2404 => UBUNTU_2404,
        RunnerImage::Ubuntu2204 => UBUNTU_2204,
    }
}
