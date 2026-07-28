//! Host-platform validation and the runner-label → base-image map.
//!
//! v0 hosts are Linux x86_64 only (`greenlit-v0-spec.md` "Tech": "Host
//! platform: Linux x86_64 only for v0"). Everything the engine does is gated on
//! [`validate_host`] first, so a wrong-platform run fails with one clear action
//! rather than an opaque bollard error deep in image work.
//!
//! The label map is the runtime-side counterpart to `greenlit-engine`'s
//! `resolve_runner_label`: planning validates and rejects `runs-on:` labels,
//! and here we translate an already-accepted label into the concrete Ubuntu
//! release the base image is built from. `ubuntu-latest`, `ubuntu-24.04`, and
//! the KanterLabs `homelab` runner tiers map to 24.04; `ubuntu-22.04` maps to
//! 22.04.

/// The host is not a v0-supported platform.
///
/// v0 supports Linux x86_64 only; other operating systems and CPU
/// architectures are ports behind the [`crate::ContainerEngine`] trait, not
/// runtime failures to paper over.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct UnsupportedHost {
    /// What is wrong, phrased for the operator.
    message: String,
    /// The one action that resolves it.
    fix: String,
}

impl UnsupportedHost {
    /// The one action that resolves the platform mismatch.
    #[must_use]
    pub fn fix(&self) -> &str {
        &self.fix
    }
}

/// Validates that the current host is Linux x86_64.
///
/// Returns [`UnsupportedHost`] with a fix action on any other OS or
/// architecture. Uses [`std::env::consts`], which the compiler fills in at
/// build time from the target triple.
///
/// # Errors
///
/// Returns [`UnsupportedHost`] when the OS is not `linux` or the architecture
/// is not `x86_64`.
pub fn validate_host() -> Result<(), UnsupportedHost> {
    check_host(std::env::consts::OS, std::env::consts::ARCH)
}

/// Pure core of [`validate_host`], parameterised on OS/arch so the mismatch
/// branches are exercised on any build host.
fn check_host(os: &str, arch: &str) -> Result<(), UnsupportedHost> {
    if os != "linux" {
        return Err(UnsupportedHost {
            message: format!(
                "Greenlit v0 runs on Linux x86_64 hosts only; this host reports OS `{os}`."
            ),
            // WSL2 on x86_64 is the documented path for Windows users
            // (`greenlit-v0-spec.md` "Tech").
            fix: "run Greenlit on a Linux x86_64 host (WSL2 works for Windows)".to_string(),
        });
    }
    if arch != "x86_64" {
        return Err(UnsupportedHost {
            message: format!(
                "Greenlit v0 runs on x86_64 only; this host reports architecture `{arch}`."
            ),
            fix: "run Greenlit on an x86_64 host; other architectures are a future port"
                .to_string(),
        });
    }
    Ok(())
}

/// The Ubuntu release a supported runner label resolves to.
///
/// This is the base an image is built from — `ubuntu:24.04` / `ubuntu:22.04` on
/// Docker Hub — not the `runs-on:` label itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UbuntuRelease {
    /// Ubuntu 24.04 LTS — the target of hosted 24.04 and both homelab tiers.
    Noble2404,
    /// Ubuntu 22.04 LTS — the target of `ubuntu-22.04`.
    Jammy2204,
}

impl UbuntuRelease {
    /// The `MAJOR.MINOR` release string (`"24.04"`, `"22.04"`).
    #[must_use]
    pub fn version(self) -> &'static str {
        match self {
            UbuntuRelease::Noble2404 => "24.04",
            UbuntuRelease::Jammy2204 => "22.04",
        }
    }

    /// The upstream Docker Hub base-image reference (`ubuntu:24.04`).
    ///
    /// The convergent Greenlit base image (`greenlit/*`, built in a later Phase
    /// 2 task group) is derived `FROM` this.
    #[must_use]
    pub fn base_image_ref(self) -> &'static str {
        match self {
            UbuntuRelease::Noble2404 => "ubuntu:24.04",
            UbuntuRelease::Jammy2204 => "ubuntu:22.04",
        }
    }
}

/// Maps a supported `runs-on:` label to its Ubuntu release.
///
/// Returns `None` for every unsupported label. Planning in `greenlit-engine`
/// already rejects unsupported labels with a source-spanned message; this map
/// is the defense-in-depth runtime translation and accepts the hosted Ubuntu
/// labels plus the KanterLabs `homelab` Ubuntu alias.
/// `ubuntu-latest` resolves to 24.04 for v0.
#[must_use]
pub fn resolve_base_image(label: &str) -> Option<UbuntuRelease> {
    match label {
        "ubuntu-latest" | "ubuntu-24.04" | "homelab" | "homelab-heavy" => {
            Some(UbuntuRelease::Noble2404)
        }
        "ubuntu-22.04" => Some(UbuntuRelease::Jammy2204),
        _ => None,
    }
}
