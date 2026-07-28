//! Phase 12 planning inventory for the `${{ vars.* }}` context.
//!
//! Variable acquisition and resolution are intentionally absent. Reachable
//! variable use is planned with opaque placeholders, retained as a
//! non-forceable `variable.context` finding, and blocked before any process
//! environment, repository file, credential, or network source is consulted.
//! Phase 16 owns the trust-scoped input preflight that can reintroduce a
//! resolver.

/// Variable references whose authored conditions must remain reachable until
/// the Phase 16 preflight can resolve their exact values.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UnresolvedPlanningVars {
    pub(crate) names: Vec<String>,
    pub(crate) has_dynamic_lookup: bool,
}

/// The compiled CLI still validates explicit `--var` names before accepting
/// their values into the in-memory masking registry.
pub(crate) use crate::gh_names::validate_configuration_name as validate_name;
