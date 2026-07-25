//! OCI Distribution API resolution for Linux amd64 images.
//!
//! The protocol follows the OCI Distribution Specification pull flow:
//! manifests are requested at `/v2/<name>/manifests/<reference>` with explicit
//! accepted media types, every returned byte stream is verified by digest,
//! and an image index is resolved to one platform manifest before execution.
//! See <https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pull>.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use crate::cas::{CasError, CasStore, ObjectDigest};

const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 16 * 1024 * 1024;
const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json, ",
    "application/vnd.oci.image.manifest.v1+json, ",
    "application/vnd.docker.distribution.manifest.list.v2+json, ",
    "application/vnd.docker.distribution.manifest.v2+json"
);

/// One selected immutable image manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    /// Original authored reference.
    pub requested: String,
    /// Repository reference pinned to the selected platform digest.
    pub pull_reference: String,
    /// Selected platform-manifest digest.
    pub digest: ObjectDigest,
    /// Config-declared operating system.
    pub os: String,
    /// Config-declared architecture.
    pub architecture: String,
    /// Whether verified manifest/config bytes came entirely from the CAS
    /// after the registry's zero-body alias recheck.
    pub cache_hit: bool,
    /// Whether every filesystem layer carries a verified eStargz TOC
    /// annotation and can therefore be mounted and chunk-verified lazily.
    pub lazy_compatible: bool,
}

/// Registry parsing, authentication, protocol, or verification failure.
#[derive(Debug, thiserror::Error)]
pub enum OciError {
    /// Authored image reference is malformed.
    #[error("invalid OCI image reference '{reference}': {reason}")]
    InvalidReference {
        /// Rejected reference.
        reference: String,
        /// Specific syntax problem.
        reason: String,
    },
    /// Registry request failed or returned an unsuccessful status.
    #[error("OCI registry request {url}: {message}")]
    Request {
        /// Requested URL.
        url: String,
        /// Transport or status detail.
        message: String,
    },
    /// Authentication challenge or token response is unusable.
    #[error("OCI registry authentication for {registry}: {message}")]
    Authentication {
        /// Registry host.
        registry: String,
        /// Protocol detail.
        message: String,
    },
    /// Manifest/config JSON is malformed.
    #[error("OCI metadata for {reference}: {message}")]
    Metadata {
        /// Requested image.
        reference: String,
        /// Parse or shape detail.
        message: String,
    },
    /// No requested platform exists in an image index.
    #[error("OCI image '{reference}' has no linux/amd64 platform manifest")]
    PlatformMissing {
        /// Requested image.
        reference: String,
    },
    /// Registry bytes do not match their required identity.
    #[error("OCI content from {url} does not match {expected}; computed {actual}")]
    DigestMismatch {
        /// Object URL.
        url: String,
        /// Descriptor or reference identity.
        expected: ObjectDigest,
        /// Computed identity.
        actual: ObjectDigest,
    },
    /// Verified CAS failed.
    #[error(transparent)]
    Content(#[from] CasError),
    /// Offline mode requires registry metadata absent from the CAS.
    #[error("offline content is missing: OCI image {reference}")]
    OfflineMissing {
        /// Missing locked image reference.
        reference: String,
    },
}

/// Resolves image aliases through a registry and publishes metadata objects
/// into the machine-wide verified content store.
#[derive(Debug, Clone)]
pub struct RegistryResolver {
    store: CasStore,
    agent: ureq::Agent,
}

impl RegistryResolver {
    /// Creates a resolver backed by `store`.
    #[must_use]
    pub fn new(store: CasStore) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build();
        Self {
            store,
            agent: config.into(),
        }
    }

    /// Resolves `reference` to its Linux amd64 platform manifest and verifies
    /// that manifest's config identity and platform.
    pub fn resolve_linux_amd64(&self, reference: &str) -> Result<ResolvedImage, OciError> {
        let parsed = RegistryReference::parse(reference)?;
        let probe = self.authenticate(&parsed)?;
        if let Some(top_digest) = probe.digest.as_ref()
            && let Some(cached) = self.cached_resolution(&parsed, reference, top_digest)?
        {
            return Ok(cached);
        }
        let top = self.fetch_manifest(&parsed, &parsed.reference, probe.token.as_deref())?;
        let expected_top = probe.digest.as_ref().or(top.expected.as_ref());
        let top_digest = verified_digest(&top.url, expected_top, &top.bytes)?;
        self.store.put_verified(&top_digest, &top.bytes)?;
        let envelope: ManifestEnvelope =
            serde_json::from_slice(&top.bytes).map_err(|error| OciError::Metadata {
                reference: reference.to_string(),
                message: error.to_string(),
            })?;

        let selected = if envelope.manifests.is_empty() {
            (top_digest.clone(), top.bytes)
        } else {
            let descriptor = envelope
                .manifests
                .iter()
                .find(|descriptor| {
                    descriptor.platform.as_ref().is_some_and(|platform| {
                        platform.os == "linux"
                            && (platform.architecture == "amd64"
                                || platform.architecture == "x86_64")
                    })
                })
                .ok_or_else(|| OciError::PlatformMissing {
                    reference: reference.to_string(),
                })?;
            let digest =
                ObjectDigest::parse(&descriptor.digest).map_err(|error| OciError::Metadata {
                    reference: reference.to_string(),
                    message: format!("platform descriptor digest: {error}"),
                })?;
            let child = self.fetch_manifest(&parsed, digest.as_str(), probe.token.as_deref())?;
            verify_size(
                reference,
                "platform manifest",
                descriptor.size,
                child.bytes.len(),
            )?;
            let actual = verified_digest(&child.url, Some(&digest), &child.bytes)?;
            self.store.put_verified(&actual, &child.bytes)?;
            (actual, child.bytes)
        };

        let manifest: ImageManifest =
            serde_json::from_slice(&selected.1).map_err(|error| OciError::Metadata {
                reference: reference.to_string(),
                message: error.to_string(),
            })?;
        let lazy_compatible = manifest.lazy_compatible();
        let config_digest =
            ObjectDigest::parse(&manifest.config.digest).map_err(|error| OciError::Metadata {
                reference: reference.to_string(),
                message: format!("config descriptor digest: {error}"),
            })?;
        let config_url = parsed.blob_url(config_digest.as_str());
        let config_bytes = self.fetch(
            &config_url,
            "application/octet-stream",
            probe.token.as_deref(),
            MAX_CONFIG_BYTES,
        )?;
        verify_size(
            reference,
            "image config",
            manifest.config.size,
            config_bytes.len(),
        )?;
        let actual_config = verified_digest(&config_url, Some(&config_digest), &config_bytes)?;
        self.store.put_verified(&actual_config, &config_bytes)?;
        let image_config: ImageConfig =
            serde_json::from_slice(&config_bytes).map_err(|error| OciError::Metadata {
                reference: reference.to_string(),
                message: format!("image config: {error}"),
            })?;
        if image_config.os != "linux"
            || (image_config.architecture != "amd64" && image_config.architecture != "x86_64")
        {
            return Err(OciError::PlatformMissing {
                reference: reference.to_string(),
            });
        }
        self.store.record_alias("oci-top", reference, &top_digest)?;
        self.store
            .record_alias("oci-linux-amd64", reference, &selected.0)?;
        Ok(ResolvedImage {
            requested: reference.to_string(),
            pull_reference: format!("{}@{}", parsed.pull_name, selected.0),
            digest: selected.0,
            os: image_config.os,
            architecture: image_config.architecture,
            cache_hit: false,
            lazy_compatible,
        })
    }

    /// Resolves a previously verified Linux amd64 image without making a
    /// registry request.
    pub fn resolve_linux_amd64_offline(&self, reference: &str) -> Result<ResolvedImage, OciError> {
        let parsed = RegistryReference::parse(reference)?;
        let top = self
            .store
            .resolve_alias("oci-top", reference)?
            .ok_or_else(|| OciError::OfflineMissing {
                reference: reference.to_string(),
            })?;
        self.cached_resolution(&parsed, reference, &top)?
            .ok_or_else(|| OciError::OfflineMissing {
                reference: reference.to_string(),
            })
    }

    fn cached_resolution(
        &self,
        parsed: &RegistryReference,
        requested: &str,
        top_digest: &ObjectDigest,
    ) -> Result<Option<ResolvedImage>, OciError> {
        if self.store.resolve_alias("oci-top", requested)?.as_ref() != Some(top_digest) {
            return Ok(None);
        }
        let Some(selected) = self.store.resolve_alias("oci-linux-amd64", requested)? else {
            return Ok(None);
        };
        let Some(manifest_bytes) = read_cache_object(&self.store, &selected)? else {
            return Ok(None);
        };
        let manifest: ImageManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|error| OciError::Metadata {
                reference: requested.to_string(),
                message: error.to_string(),
            })?;
        let lazy_compatible = manifest.lazy_compatible();
        let config_digest =
            ObjectDigest::parse(&manifest.config.digest).map_err(|error| OciError::Metadata {
                reference: requested.to_string(),
                message: format!("config descriptor digest: {error}"),
            })?;
        let Some(config_bytes) = read_cache_object(&self.store, &config_digest)? else {
            return Ok(None);
        };
        let image_config: ImageConfig =
            serde_json::from_slice(&config_bytes).map_err(|error| OciError::Metadata {
                reference: requested.to_string(),
                message: format!("image config: {error}"),
            })?;
        Ok(Some(ResolvedImage {
            requested: requested.to_string(),
            pull_reference: format!("{}@{}", parsed.pull_name, selected),
            digest: selected,
            os: image_config.os,
            architecture: image_config.architecture,
            cache_hit: true,
            lazy_compatible,
        }))
    }

    fn authenticate(&self, reference: &RegistryReference) -> Result<AuthSession, OciError> {
        let url = reference.manifest_url(&reference.reference);
        let response = self
            .agent
            .head(&url)
            .header("Accept", ACCEPT_MANIFESTS)
            .header("User-Agent", user_agent())
            .call()
            .map_err(|error| request_error(&url, error.to_string()))?;
        if response.status().is_success() {
            return Ok(AuthSession {
                token: None,
                digest: response_digest(
                    &url,
                    response
                        .headers()
                        .get("docker-content-digest")
                        .and_then(|value| value.to_str().ok()),
                )?,
            });
        }
        if response.status().as_u16() != 401 {
            return Err(request_error(
                &url,
                format!("HTTP status {}", response.status().as_u16()),
            ));
        }
        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| OciError::Authentication {
                registry: reference.registry.clone(),
                message: "401 response omitted WWW-Authenticate".to_string(),
            })?;
        let challenge =
            BearerChallenge::parse(challenge).ok_or_else(|| OciError::Authentication {
                registry: reference.registry.clone(),
                message: "unsupported or malformed authentication challenge".to_string(),
            })?;
        let mut request = self
            .agent
            .get(&challenge.realm)
            .header("User-Agent", user_agent());
        if let Some(service) = challenge.service.as_deref() {
            request = request.query("service", service);
        }
        let scope = challenge
            .scope
            .unwrap_or_else(|| format!("repository:{}:pull", reference.repository));
        request = request.query("scope", &scope);
        let mut response = request.call().map_err(|error| OciError::Authentication {
            registry: reference.registry.clone(),
            message: error.to_string(),
        })?;
        if !response.status().is_success() {
            return Err(OciError::Authentication {
                registry: reference.registry.clone(),
                message: format!("token service returned HTTP {}", response.status().as_u16()),
            });
        }
        let token: TokenResponse =
            response
                .body_mut()
                .read_json()
                .map_err(|error| OciError::Authentication {
                    registry: reference.registry.clone(),
                    message: error.to_string(),
                })?;
        let token = token
            .token
            .or(token.access_token)
            .ok_or_else(|| OciError::Authentication {
                registry: reference.registry.clone(),
                message: "token response omitted token".to_string(),
            })?;
        let digest = self.authenticated_head_digest(reference, &url, &token)?;
        Ok(AuthSession {
            token: Some(token),
            digest,
        })
    }

    fn authenticated_head_digest(
        &self,
        reference: &RegistryReference,
        url: &str,
        token: &str,
    ) -> Result<Option<ObjectDigest>, OciError> {
        let response = self
            .agent
            .head(url)
            .header("Accept", ACCEPT_MANIFESTS)
            .header("User-Agent", user_agent())
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .map_err(|error| request_error(url, error.to_string()))?;
        if !response.status().is_success() {
            return Err(OciError::Authentication {
                registry: reference.registry.clone(),
                message: format!(
                    "authenticated manifest probe returned HTTP {}",
                    response.status().as_u16()
                ),
            });
        }
        response_digest(
            url,
            response
                .headers()
                .get("docker-content-digest")
                .and_then(|value| value.to_str().ok()),
        )
    }

    fn fetch_manifest(
        &self,
        reference: &RegistryReference,
        manifest_reference: &str,
        token: Option<&str>,
    ) -> Result<Fetched, OciError> {
        let url = reference.manifest_url(manifest_reference);
        let bytes = self.fetch(&url, ACCEPT_MANIFESTS, token, MAX_MANIFEST_BYTES)?;
        let expected = if manifest_reference.starts_with("sha256:") {
            Some(
                ObjectDigest::parse(manifest_reference).map_err(|error| OciError::Metadata {
                    reference: reference.original.clone(),
                    message: error.to_string(),
                })?,
            )
        } else {
            None
        };
        Ok(Fetched {
            url,
            expected,
            bytes,
        })
    }

    fn fetch(
        &self,
        url: &str,
        accept: &str,
        token: Option<&str>,
        limit: u64,
    ) -> Result<Vec<u8>, OciError> {
        let mut request = self
            .agent
            .get(url)
            .header("Accept", accept)
            .header("User-Agent", user_agent());
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let mut response = request
            .call()
            .map_err(|error| request_error(url, error.to_string()))?;
        if !response.status().is_success() {
            return Err(request_error(
                url,
                format!("HTTP status {}", response.status().as_u16()),
            ));
        }
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|error| request_error(url, error.to_string()))
    }
}

#[derive(Debug)]
struct Fetched {
    url: String,
    expected: Option<ObjectDigest>,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct AuthSession {
    token: Option<String>,
    digest: Option<ObjectDigest>,
}

#[derive(Debug)]
struct RegistryReference {
    original: String,
    registry: String,
    api_registry: String,
    repository: String,
    reference: String,
    pull_name: String,
    scheme: &'static str,
}

impl RegistryReference {
    fn parse(value: &str) -> Result<Self, OciError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(invalid_ref(value, "reference is empty"));
        }
        let (name, reference) = if let Some((name, digest)) = value.rsplit_once('@') {
            (name, digest)
        } else {
            let last_slash = value.rfind('/');
            let last_colon = value.rfind(':');
            if last_colon.is_some_and(|colon| last_slash.is_none_or(|slash| colon > slash)) {
                let colon = last_colon.unwrap_or_default();
                (&value[..colon], &value[colon + 1..])
            } else {
                (value, "latest")
            }
        };
        if name.is_empty() || reference.is_empty() {
            return Err(invalid_ref(value, "name and tag/digest must be non-empty"));
        }
        let mut components = name.split('/');
        let first = components.next().unwrap_or_default();
        let explicit_registry = first.contains('.') || first.contains(':') || first == "localhost";
        let (registry, repository, pull_name) = if explicit_registry {
            let repository = components.collect::<Vec<_>>().join("/");
            if repository.is_empty() {
                return Err(invalid_ref(value, "repository name is missing"));
            }
            (first.to_string(), repository, name.to_string())
        } else {
            let repository = if name.contains('/') {
                name.to_string()
            } else {
                format!("library/{name}")
            };
            (
                "docker.io".to_string(),
                repository.clone(),
                format!("docker.io/{repository}"),
            )
        };
        if !repository.split('/').all(valid_repository_component) {
            return Err(invalid_ref(
                value,
                "repository must use lowercase registry path characters",
            ));
        }
        let api_registry = if registry == "docker.io" {
            "registry-1.docker.io".to_string()
        } else {
            registry.clone()
        };
        let scheme = if registry == "localhost"
            || registry.starts_with("localhost:")
            || registry.starts_with("127.")
            || registry.starts_with("[::1]")
        {
            "http"
        } else {
            "https"
        };
        Ok(Self {
            original: value.to_string(),
            registry,
            api_registry,
            repository,
            reference: reference.to_string(),
            pull_name,
            scheme,
        })
    }

    fn manifest_url(&self, reference: &str) -> String {
        format!(
            "{}://{}/v2/{}/manifests/{reference}",
            self.scheme, self.api_registry, self.repository
        )
    }

    fn blob_url(&self, digest: &str) -> String {
        format!(
            "{}://{}/v2/{}/blobs/{digest}",
            self.scheme, self.api_registry, self.repository
        )
    }
}

fn valid_repository_component(component: &str) -> bool {
    let mut bytes = component.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    let valid_edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    valid_edge(first)
        && component
            .as_bytes()
            .last()
            .is_some_and(|byte| valid_edge(*byte))
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

#[derive(Debug)]
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

impl BearerChallenge {
    fn parse(value: &str) -> Option<Self> {
        let parameters = value.strip_prefix("Bearer ")?;
        let mut realm = None;
        let mut service = None;
        let mut scope = None;
        for parameter in split_challenge_parameters(parameters) {
            let (name, value) = parameter.split_once('=')?;
            let value = value.trim().trim_matches('"').to_string();
            match name.trim() {
                "realm" => realm = Some(value),
                "service" => service = Some(value),
                "scope" => scope = Some(value),
                _ => {}
            }
        }
        Some(Self {
            realm: realm?,
            service,
            scope,
        })
    }
}

fn split_challenge_parameters(value: &str) -> Vec<&str> {
    let mut parameters = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                parameters.push(value[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    parameters.push(value[start..].trim());
    parameters
}

fn verified_digest(
    url: &str,
    expected: Option<&ObjectDigest>,
    bytes: &[u8],
) -> Result<ObjectDigest, OciError> {
    let actual = ObjectDigest::of_bytes(bytes);
    if let Some(expected) = expected
        && expected != &actual
    {
        return Err(OciError::DigestMismatch {
            url: url.to_string(),
            expected: expected.clone(),
            actual,
        });
    }
    Ok(actual)
}

fn verify_size(
    reference: &str,
    object: &str,
    expected: u64,
    actual: usize,
) -> Result<(), OciError> {
    if u64::try_from(actual).ok() != Some(expected) {
        return Err(OciError::Metadata {
            reference: reference.to_string(),
            message: format!(
                "{object} size does not match its descriptor (expected {expected}, got {actual})"
            ),
        });
    }
    Ok(())
}

fn response_digest(url: &str, value: Option<&str>) -> Result<Option<ObjectDigest>, OciError> {
    value
        .map(|value| {
            ObjectDigest::parse(value).map_err(|error| OciError::Metadata {
                reference: url.to_string(),
                message: format!("Docker-Content-Digest header: {error}"),
            })
        })
        .transpose()
}

fn read_cache_object(store: &CasStore, digest: &ObjectDigest) -> Result<Option<Vec<u8>>, OciError> {
    match store.read_verified(digest) {
        Ok(bytes) => Ok(bytes),
        Err(CasError::DigestMismatch { .. }) => Ok(None),
        Err(error) => Err(OciError::Content(error)),
    }
}

fn invalid_ref(reference: &str, reason: &str) -> OciError {
    OciError::InvalidReference {
        reference: reference.to_string(),
        reason: reason.to_string(),
    }
}

fn request_error(url: &str, message: String) -> OciError {
    OciError::Request {
        url: url.to_string(),
        message,
    }
}

fn user_agent() -> &'static str {
    concat!("greenlit-litci/", env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Deserialize)]
struct ManifestEnvelope {
    #[serde(default)]
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    digest: String,
    size: u64,
    platform: Option<Platform>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}

#[derive(Debug, Deserialize)]
struct ImageManifest {
    config: Descriptor,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

impl ImageManifest {
    fn lazy_compatible(&self) -> bool {
        !self.layers.is_empty()
            && self.layers.iter().all(|layer| {
                layer
                    .annotations
                    .get("containerd.io/snapshot/stargz/toc.digest")
                    .is_some_and(|digest| digest.starts_with("sha256:") && digest.len() == 71)
            })
    }
}

#[derive(Debug, Deserialize)]
struct ImageConfig {
    architecture: String,
    os: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}
