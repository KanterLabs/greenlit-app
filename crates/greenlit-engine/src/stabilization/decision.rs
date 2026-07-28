use crate::evidence::{FeatureFinding, FindingDisposition};

use super::registry::{
    CapabilityClass, CertificationState, Forceability, capability_certification,
};

const UNKNOWN_USER_ACTION: &str =
    "update Greenlit to a version that recognizes and certifies this capability before running it";

/// One source-located capability required by selected reachable work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityFinding {
    capability_id: String,
    scope: String,
    reason: String,
}

impl CapabilityFinding {
    /// Creates a source-located capability finding.
    #[must_use]
    pub fn new(
        capability_id: impl Into<String>,
        scope: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            capability_id: capability_id.into(),
            scope: scope.into(),
            reason: reason.into(),
        }
    }

    /// Stable capability identifier supplied by the analyzer.
    #[must_use]
    pub fn capability_id(&self) -> &str {
        &self.capability_id
    }

    /// Source location or exact logical scope affected by the finding.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Exact explanation of why the capability is required.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// One finding resolved against the authoritative registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCapabilityFinding {
    finding: CapabilityFinding,
    owning_phase: Option<u8>,
    state: CertificationState,
    class: CapabilityClass,
    forceability: Forceability,
    user_action: &'static str,
}

impl ResolvedCapabilityFinding {
    /// Original exact source-located finding.
    #[must_use]
    pub fn finding(&self) -> &CapabilityFinding {
        &self.finding
    }

    /// Owning stabilization phase, or `None` for an unknown identifier.
    #[must_use]
    pub const fn owning_phase(&self) -> Option<u8> {
        self.owning_phase
    }

    /// Certification state used for the decision.
    #[must_use]
    pub const fn state(&self) -> CertificationState {
        self.state
    }

    /// Security relevance used for the decision.
    #[must_use]
    pub const fn class(&self) -> CapabilityClass {
        self.class
    }

    /// Effective forceability used for the decision.
    #[must_use]
    pub const fn forceability(&self) -> Forceability {
        self.forceability
    }

    /// Exact user action that resolves the block.
    #[must_use]
    pub const fn user_action(&self) -> &'static str {
        self.user_action
    }
}

/// Overall pre-execution quarantine outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineOutcome {
    /// No uncertified capability is required.
    Allowed,
    /// Every uncertified finding was explicitly and safely forced.
    Degraded,
    /// At least one finding remains blocked.
    Blocked,
}

/// Pure quarantine decision with exact forced and blocking findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineDecision {
    outcome: QuarantineOutcome,
    forced_findings: Vec<ResolvedCapabilityFinding>,
    blocking_findings: Vec<ResolvedCapabilityFinding>,
}

impl QuarantineDecision {
    /// Overall decision.
    #[must_use]
    pub const fn outcome(&self) -> QuarantineOutcome {
        self.outcome
    }

    /// Exact findings accepted by an explicit degraded override.
    #[must_use]
    pub fn forced_findings(&self) -> &[ResolvedCapabilityFinding] {
        &self.forced_findings
    }

    /// Exact findings that still block execution.
    #[must_use]
    pub fn blocking_findings(&self) -> &[ResolvedCapabilityFinding] {
        &self.blocking_findings
    }

    /// Converts only actually forced findings into degraded support evidence.
    #[must_use]
    pub fn forced_feature_findings(&self) -> Vec<FeatureFinding> {
        self.forced_findings
            .iter()
            .map(|resolved| FeatureFinding {
                code: resolved.finding.capability_id.clone(),
                disposition: FindingDisposition::Degraded,
                scope: resolved.finding.scope.clone(),
                reason: resolved.finding.reason.clone(),
            })
            .collect()
    }
}

/// Applies the authoritative capability registry to source-located findings.
///
/// Input order and duplicates are preserved so returned forced findings map
/// exactly to the analyzer's findings. Unknown identifiers and protected
/// classes are never forceable.
#[must_use]
pub fn decide_capability_quarantine(
    findings: &[CapabilityFinding],
    allow_degraded: bool,
) -> QuarantineDecision {
    let mut forced_findings = Vec::new();
    let mut blocking_findings = Vec::new();

    for finding in findings {
        let resolved = resolve_finding(finding);
        match resolved.state {
            CertificationState::Certified => {}
            CertificationState::Uncertified
                if allow_degraded && resolved.forceability == Forceability::Forceable =>
            {
                forced_findings.push(resolved);
            }
            CertificationState::Uncertified | CertificationState::Unknown => {
                blocking_findings.push(resolved);
            }
        }
    }

    let outcome = if !blocking_findings.is_empty() {
        QuarantineOutcome::Blocked
    } else if !forced_findings.is_empty() {
        QuarantineOutcome::Degraded
    } else {
        QuarantineOutcome::Allowed
    };
    QuarantineDecision {
        outcome,
        forced_findings,
        blocking_findings,
    }
}

fn resolve_finding(finding: &CapabilityFinding) -> ResolvedCapabilityFinding {
    match capability_certification(&finding.capability_id) {
        Some(certification) => ResolvedCapabilityFinding {
            finding: finding.clone(),
            owning_phase: Some(certification.owning_phase()),
            state: certification.state(),
            class: certification.class(),
            forceability: certification.forceability(),
            user_action: certification.user_action(),
        },
        None => ResolvedCapabilityFinding {
            finding: finding.clone(),
            owning_phase: None,
            state: CertificationState::Unknown,
            class: CapabilityClass::Unknown,
            forceability: Forceability::NonForceable,
            user_action: UNKNOWN_USER_ACTION,
        },
    }
}
