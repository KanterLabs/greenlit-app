//! The workflow `permissions:` vs. local-token-grant notice.
//!
//! `PHASE-3-actions.md` Auth: "The workflow `permissions:` block cannot
//! narrow a locally supplied token. Parse it; when a workflow requests more
//! than the token grants or more than `contents: read`, print one
//! actionable notice naming the difference. Document the limitation."
//!
//! On real GitHub, `permissions:` tells the platform which scopes to mint
//! the job's auto-generated `GITHUB_TOKEN` with — the token is created
//! *from* the declaration. Greenlit's local token (device-flow, pasted PAT,
//! or `gh`-sourced) already exists before any workflow is read and is fixed
//! for the whole run; there is no mechanism to mint a second, differently
//! scoped token per job the way GitHub's runner does. Concretely: `litci
//! auth`'s device flow and the PAT guidance both request read-only
//! repository access (`contents: read` plus `Variables` read), so a
//! workflow whose `permissions:` requests anything beyond that — any write
//! level, or any scope besides `contents` — is requesting something the
//! locally injected token structurally cannot provide, and steps relying on
//! that excess access will fail exactly as they would with any other
//! insufficiently scoped token.

use greenlit_engine::{PermissionLevelPlan, PermissionsPlan};

/// Every scope name other than `contents` that GitHub's `permissions:`
/// schema recognizes, transcribed from the same table
/// `greenlit-workflow`'s parser validates against
/// (`crates/greenlit-workflow/src/parse/workflow.rs::PERMISSION_SCOPES`).
/// `read-all`/`write-all` requests every one of these (plus `contents`), so
/// this list is what a `read-all` declaration exceeds the local token's
/// `contents`-only read grant with.
const SCOPES_BEYOND_CONTENTS: &[&str] = &[
    "actions",
    "artifact-metadata",
    "attestations",
    "checks",
    "code-quality",
    "deployments",
    "discussions",
    "id-token",
    "issues",
    "models",
    "packages",
    "pages",
    "pull-requests",
    "security-events",
    "statuses",
    "vulnerability-alerts",
];

/// Computes the single combined notice for a run, given each job's
/// *effective* `permissions:` (its own job-level declaration if present,
/// else the workflow-level one — job-level `permissions:` replaces rather
/// than merges with the workflow-level declaration, matching GitHub; the
/// caller resolves that before calling in). Returns `None` when every job
/// requests no more than `contents: read` (including no declaration at
/// all).
pub(crate) fn token_permission_notice(
    effective_per_job: &[Option<&PermissionsPlan>],
) -> Option<String> {
    let mut wants_write = false;
    let mut extra_scopes: Vec<&str> = Vec::new();

    for permissions in effective_per_job.iter().filter_map(|p| *p) {
        match permissions {
            PermissionsPlan::WriteAll => wants_write = true,
            PermissionsPlan::ReadAll => {
                for scope in SCOPES_BEYOND_CONTENTS {
                    if !extra_scopes.contains(scope) {
                        extra_scopes.push(scope);
                    }
                }
            }
            PermissionsPlan::Scoped { scopes } => {
                for (name, level) in scopes {
                    match (name.as_str(), level) {
                        (_, PermissionLevelPlan::None) => {}
                        ("contents", PermissionLevelPlan::Write) => wants_write = true,
                        ("contents", PermissionLevelPlan::Read) => {}
                        (other, _) => {
                            if let Some(&scope) = SCOPES_BEYOND_CONTENTS
                                .iter()
                                .find(|candidate| **candidate == other)
                                && !extra_scopes.contains(&scope)
                            {
                                extra_scopes.push(scope);
                            }
                        }
                    }
                }
            }
        }
    }

    if !wants_write && extra_scopes.is_empty() {
        return None;
    }

    let mut detail = Vec::new();
    if wants_write {
        detail.push(
            "write access to 'contents' (or every scope, via read-all/write-all)".to_string(),
        );
    }
    if !extra_scopes.is_empty() {
        detail.push(format!("read access to {}", extra_scopes.join(", ")));
    }
    Some(format!(
        "this workflow's `permissions:` requests {} beyond what a locally supplied token can grant (read-only, limited to repository contents and configuration variables); a workflow's `permissions:` block cannot narrow or widen a local token — steps that rely on the excess access will fail\n  fix: none available in v0; the workflow will only get the read-only local token regardless of its `permissions:` declaration",
        detail.join(" and ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn scoped(entries: &[(&str, PermissionLevelPlan)]) -> PermissionsPlan {
        PermissionsPlan::Scoped {
            scopes: entries
                .iter()
                .map(|(name, level)| ((*name).to_string(), *level))
                .collect::<IndexMap<_, _>>(),
        }
    }

    #[test]
    fn contents_read_only_never_notices() {
        let contents_read = scoped(&[("contents", PermissionLevelPlan::Read)]);
        assert!(token_permission_notice(&[Some(&contents_read)]).is_none());
    }

    #[test]
    fn no_declared_permissions_never_notices() {
        assert!(token_permission_notice(&[None]).is_none());
    }

    #[test]
    fn write_all_notices_write_access() {
        let notice = token_permission_notice(&[Some(&PermissionsPlan::WriteAll)]).expect("notice");
        assert!(notice.contains("write access"), "{notice}");
        assert!(notice.contains("cannot narrow"), "{notice}");
    }

    #[test]
    fn read_all_notices_the_extra_scopes() {
        let notice = token_permission_notice(&[Some(&PermissionsPlan::ReadAll)]).expect("notice");
        assert!(notice.contains("issues"), "{notice}");
        assert!(notice.contains("packages"), "{notice}");
    }

    #[test]
    fn an_extra_scoped_read_permission_notices_it_by_name() {
        let issues_read = scoped(&[("issues", PermissionLevelPlan::Read)]);
        let notice = token_permission_notice(&[Some(&issues_read)]).expect("notice");
        assert!(notice.contains("issues"), "{notice}");
    }

    #[test]
    fn contents_write_notices_write_access_even_when_scoped() {
        let contents_write = scoped(&[("contents", PermissionLevelPlan::Write)]);
        let notice = token_permission_notice(&[Some(&contents_write)]).expect("notice");
        assert!(notice.contains("write access"), "{notice}");
    }

    #[test]
    fn any_exceeding_job_is_enough_to_notice_once() {
        let contents_read = scoped(&[("contents", PermissionLevelPlan::Read)]);
        let issues_read = scoped(&[("issues", PermissionLevelPlan::Read)]);
        let notice =
            token_permission_notice(&[Some(&contents_read), Some(&issues_read)]).expect("notice");
        assert!(notice.contains("issues"), "{notice}");
    }
}
