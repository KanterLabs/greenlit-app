/// Uncertified baseline shell-step execution.
pub const CAPABILITY_EXECUTION_SHELL: &str = "execution.shell";
/// GitHub credential access or injection.
pub const CAPABILITY_CREDENTIAL_GITHUB: &str = "credential.github";
/// A reachable `secrets.*` context reference.
pub const CAPABILITY_SECRET_CONTEXT: &str = "secret.context";
/// Repository- or organization-backed configuration variable lookup.
pub const CAPABILITY_VARIABLE_REMOTE: &str = "variable.remote";
/// A reachable `uses:` action.
pub const CAPABILITY_ACTION_USES: &str = "action.uses";
/// Docker-in-Docker infrastructure.
pub const CAPABILITY_INFRASTRUCTURE_DIND: &str = "infrastructure.dind";
/// A reachable workflow service container.
pub const CAPABILITY_SERVICE_CONTAINER: &str = "service.container";
/// Source snapshot and workspace containment.
pub const CAPABILITY_SOURCE_CONTAINMENT: &str = "source.containment";
/// Applying sandbox changes back to the host source tree.
pub const CAPABILITY_SOURCE_WRITE_BACK: &str = "source.write-back";
/// Lock, result, event, trace, or other evidence integrity.
pub const CAPABILITY_EVIDENCE_INTEGRITY: &str = "evidence.integrity";
/// Reachability the current planner cannot decide statically.
pub const CAPABILITY_REACHABILITY_AMBIGUOUS: &str = "reachability.ambiguous";
/// A construct that crosses a mandatory Greenlit security boundary.
pub const CAPABILITY_SECURITY_BOUNDARY: &str = "security.boundary";

const SHELL_ACTION: &str = "rerun with `--allow-degraded` to execute without assurance";
const CREDENTIAL_ACTION: &str =
    "remove the reachable GitHub credential dependency before running locally";
const SECRET_ACTION: &str = "remove the reachable `secrets.*` reference before running locally";
const VARIABLE_ACTION: &str =
    "provide every referenced variable locally so no repository or organization lookup is required";
const USES_ACTION: &str = "remove the reachable `uses:` action before running locally";
const DIND_ACTION: &str = "remove the Docker-in-Docker requirement before running locally";
const SERVICE_ACTION: &str = "remove the reachable service container before running locally";
const SOURCE_ACTION: &str =
    "update Greenlit to a build that certifies source containment before running";
const WRITE_BACK_ACTION: &str = "omit `--write-back` before running locally";
const EVIDENCE_ACTION: &str =
    "update Greenlit to a build that certifies evidence integrity before running";
const AMBIGUITY_ACTION: &str =
    "rewrite the workflow so the selected work is statically reachable or unreachable";
const SECURITY_ACTION: &str =
    "remove the construct that crosses Greenlit's mandatory security boundary";

/// Security relevance of a capability.
///
/// The protected variants are structurally non-forceable even if a future
/// registry edit accidentally marks one forceable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityClass {
    /// Compatibility semantics without a security boundary.
    Semantics,
    /// A mandatory security boundary.
    Security,
    /// Credential access, storage, or injection.
    Credential,
    /// Secret access, storage, or injection.
    Secret,
    /// Execution of a local or remote action.
    ActionExecution,
    /// Privileged Greenlit-owned or workflow-owned infrastructure.
    PrivilegedInfrastructure,
    /// Service-container provisioning and isolation.
    ServiceInfrastructure,
    /// Source freezing, mounting, or host write containment.
    SourceContainment,
    /// Integrity of retained or rendered evidence.
    EvidenceIntegrity,
    /// A capability identifier absent from this build's registry.
    Unknown,
}

impl CapabilityClass {
    /// Returns whether policy permanently forbids degraded forcing.
    #[must_use]
    pub const fn is_protected(self) -> bool {
        !matches!(self, Self::Semantics)
    }
}

/// Current stabilization certification state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificationState {
    /// The capability has completed its owning certification phase.
    Certified,
    /// The capability is known but has not completed certification.
    Uncertified,
    /// The capability identifier is unknown to this build.
    Unknown,
}

/// Whether an uncertified capability may run without assurance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forceability {
    /// An explicit degraded override may accept the finding.
    Forceable,
    /// No override may accept the finding.
    NonForceable,
}

/// One immutable row in the authoritative capability registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityCertification {
    id: &'static str,
    owning_phase: u8,
    state: CertificationState,
    class: CapabilityClass,
    configured_forceability: Forceability,
    user_action: &'static str,
}

impl CapabilityCertification {
    const fn uncertified(
        id: &'static str,
        owning_phase: u8,
        class: CapabilityClass,
        configured_forceability: Forceability,
        user_action: &'static str,
    ) -> Self {
        Self {
            id,
            owning_phase,
            state: CertificationState::Uncertified,
            class,
            configured_forceability,
            user_action,
        }
    }

    /// Stable capability identifier.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    /// Stabilization phase that owns certification.
    #[must_use]
    pub const fn owning_phase(&self) -> u8 {
        self.owning_phase
    }

    /// Current certification state.
    #[must_use]
    pub const fn state(&self) -> CertificationState {
        self.state
    }

    /// Security relevance used by override policy.
    #[must_use]
    pub const fn class(&self) -> CapabilityClass {
        self.class
    }

    /// Effective forceability after protected-class enforcement.
    #[must_use]
    pub const fn forceability(&self) -> Forceability {
        if self.class.is_protected() {
            Forceability::NonForceable
        } else {
            self.configured_forceability
        }
    }

    /// Exact user action that resolves a default block.
    #[must_use]
    pub const fn user_action(&self) -> &'static str {
        self.user_action
    }
}

macro_rules! uncertified {
    ($id:expr, $phase:literal, $class:ident, $forceability:ident, $action:expr) => {
        CapabilityCertification::uncertified(
            $id,
            $phase,
            CapabilityClass::$class,
            Forceability::$forceability,
            $action,
        )
    };
}

const CERTIFICATIONS: &[CapabilityCertification] = &[
    uncertified!(
        CAPABILITY_EXECUTION_SHELL,
        22,
        Semantics,
        Forceable,
        SHELL_ACTION
    ),
    uncertified!(
        CAPABILITY_CREDENTIAL_GITHUB,
        16,
        Credential,
        NonForceable,
        CREDENTIAL_ACTION
    ),
    uncertified!(
        CAPABILITY_SECRET_CONTEXT,
        16,
        Secret,
        NonForceable,
        SECRET_ACTION
    ),
    uncertified!(
        CAPABILITY_VARIABLE_REMOTE,
        16,
        Credential,
        NonForceable,
        VARIABLE_ACTION
    ),
    uncertified!(
        CAPABILITY_ACTION_USES,
        23,
        ActionExecution,
        NonForceable,
        USES_ACTION
    ),
    uncertified!(
        CAPABILITY_INFRASTRUCTURE_DIND,
        20,
        PrivilegedInfrastructure,
        NonForceable,
        DIND_ACTION
    ),
    uncertified!(
        CAPABILITY_SERVICE_CONTAINER,
        24,
        ServiceInfrastructure,
        NonForceable,
        SERVICE_ACTION
    ),
    uncertified!(
        CAPABILITY_SOURCE_CONTAINMENT,
        13,
        SourceContainment,
        NonForceable,
        SOURCE_ACTION
    ),
    uncertified!(
        CAPABILITY_SOURCE_WRITE_BACK,
        26,
        SourceContainment,
        NonForceable,
        WRITE_BACK_ACTION
    ),
    uncertified!(
        CAPABILITY_EVIDENCE_INTEGRITY,
        18,
        EvidenceIntegrity,
        NonForceable,
        EVIDENCE_ACTION
    ),
    uncertified!(
        CAPABILITY_REACHABILITY_AMBIGUOUS,
        17,
        Semantics,
        NonForceable,
        AMBIGUITY_ACTION
    ),
    uncertified!(
        CAPABILITY_SECURITY_BOUNDARY,
        20,
        Security,
        NonForceable,
        SECURITY_ACTION
    ),
];

/// Returns every known capability certification in stable registry order.
#[must_use]
pub const fn capability_certifications() -> &'static [CapabilityCertification] {
    CERTIFICATIONS
}

/// Looks up one exact, case-sensitive capability identifier.
#[must_use]
pub fn capability_certification(id: &str) -> Option<&'static CapabilityCertification> {
    CERTIFICATIONS
        .iter()
        .find(|certification| certification.id == id)
}
