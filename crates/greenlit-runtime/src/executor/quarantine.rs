//! Authoritative Phase 12 quarantine at the runtime/engine boundary.
//!
//! This module derives required capabilities from the execution plan and
//! runtime configuration themselves. It never trusts a caller-provided list,
//! and rejects blocking findings before the first
//! [`ContainerEngine`](crate::engine::ContainerEngine) operation.

mod assessment;
mod findings;
mod plan_contexts;
mod reachability;

use findings::collect_job_findings;
use reachability::{condition_is_deferred, condition_is_false, selected_matrix_legs};

pub(super) use assessment::enforce_runtime_quarantine;
pub use assessment::{
    RuntimeAuthorization, RuntimeCapabilityAssessment, RuntimeCapabilityInputs, RuntimeControl,
    assess_runtime_capabilities,
};
pub use reachability::{PlanReachability, plan_reachability};
