//! Phase 12 secret-name intake.
//!
//! Secret values are not resolved while the containment quarantine is active.
//! The CLI still validates `-s NAME=VALUE` syntax so it can reject malformed
//! configuration before workflow preparation, while any reachable
//! `secrets.*` use is classified by `crate::run_quarantine`.

/// Re-exports the shared naming rule under this module's expected name.
pub(crate) use crate::gh_names::validate_configuration_name as validate_name;

/// GitHub's reserved workflow-token secret name.
pub(crate) const GITHUB_TOKEN_NAME: &str = "GITHUB_TOKEN";
