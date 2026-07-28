//! Authoritative stabilization capability certification and quarantine policy.
//!
//! A caller reports the capabilities required by the selected, reachable
//! work. This module resolves those findings against one closed registry and
//! returns a pure decision. Unknown capability identifiers fail closed.
//! `--allow-degraded` policy is represented only by the `allow_degraded`
//! argument to [`decide_capability_quarantine`]; it never changes the
//! registry and cannot force a protected capability class.

mod decision;
mod registry;

pub use decision::{
    CapabilityFinding, QuarantineDecision, QuarantineOutcome, ResolvedCapabilityFinding,
    decide_capability_quarantine,
};
pub use registry::{
    CAPABILITY_ACTION_USES, CAPABILITY_CREDENTIAL_GITHUB, CAPABILITY_DISPATCH_INPUT,
    CAPABILITY_EVIDENCE_INTEGRITY, CAPABILITY_EXECUTION_SHELL, CAPABILITY_INFRASTRUCTURE_DIND,
    CAPABILITY_REACHABILITY_AMBIGUOUS, CAPABILITY_SECRET_CONTEXT, CAPABILITY_SECURITY_BOUNDARY,
    CAPABILITY_SERVICE_CONTAINER, CAPABILITY_SOURCE_CONTAINMENT, CAPABILITY_SOURCE_WRITE_BACK,
    CAPABILITY_VARIABLE_CONTEXT, CAPABILITY_VARIABLE_REMOTE, CapabilityCertification,
    CapabilityClass, CertificationState, Forceability, capability_certification,
    capability_certifications,
};
