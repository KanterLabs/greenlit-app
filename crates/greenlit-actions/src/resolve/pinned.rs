//! One-run resolver that freezes mutable action aliases before execution.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{RefResolver, ResolveError};
use crate::CommitSha;

type RefKey = (String, String, String);

#[derive(Default)]
struct ResolutionState {
    values: BTreeMap<RefKey, CommitSha>,
    frozen: bool,
}

/// Caches action-ref resolutions for one run and can freeze them after a
/// consistency recheck. Once frozen, execution can never re-resolve a tag or
/// branch to different content.
pub struct PinnedRefResolver {
    inner: Arc<dyn RefResolver>,
    state: Mutex<ResolutionState>,
}

impl PinnedRefResolver {
    /// Wraps the real network resolver for one run.
    #[must_use]
    pub fn new(inner: Arc<dyn RefResolver>) -> Self {
        Self {
            inner,
            state: Mutex::new(ResolutionState::default()),
        }
    }

    /// Rechecks every mutable alias and then prevents any new resolution.
    ///
    /// # Errors
    /// Returns the underlying resolution failure, or fails if an alias moved
    /// between initial resolution and finalization.
    pub async fn freeze(&self) -> Result<(), ResolveError> {
        let initial = self
            .state
            .lock()
            .map_err(|_| state_error("action resolution state lock was poisoned"))?
            .values
            .clone();
        for ((owner, repo, git_ref), expected) in &initial {
            let current = self.inner.resolve(owner, repo, git_ref).await?;
            if &current != expected {
                return Err(ResolveError::Api {
                    owner: owner.clone(),
                    repo: repo.clone(),
                    git_ref: git_ref.clone(),
                    message: format!(
                        "mutable action ref changed during resolution ({} -> {})",
                        expected.as_str(),
                        current.as_str()
                    ),
                });
            }
        }
        self.state
            .lock()
            .map_err(|_| state_error("action resolution state lock was poisoned"))?
            .frozen = true;
        Ok(())
    }

    /// Returns requested repository refs mapped to their locked commits.
    ///
    /// # Errors
    /// Returns an internal-state error if the resolver state is unavailable.
    pub fn resolutions(&self) -> Result<BTreeMap<String, String>, ResolveError> {
        let state = self
            .state
            .lock()
            .map_err(|_| state_error("action resolution state lock was poisoned"))?;
        Ok(state
            .values
            .iter()
            .map(|((owner, repo, git_ref), commit)| {
                (
                    format!("{owner}/{repo}@{git_ref}"),
                    commit.as_str().to_string(),
                )
            })
            .collect())
    }
}

#[async_trait]
impl RefResolver for PinnedRefResolver {
    async fn resolve(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<CommitSha, ResolveError> {
        let key = (owner.to_string(), repo.to_string(), git_ref.to_string());
        {
            let state = self
                .state
                .lock()
                .map_err(|_| state_error("action resolution state lock was poisoned"))?;
            if let Some(commit) = state.values.get(&key) {
                return Ok(commit.clone());
            }
            if state.frozen {
                return Err(state_error(
                    "execution requested an action ref absent from the finalized RunLock",
                ));
            }
        }
        let resolved = self.inner.resolve(owner, repo, git_ref).await?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| state_error("action resolution state lock was poisoned"))?;
        if state.frozen {
            return Err(state_error(
                "action resolution finalized while a new ref was in flight",
            ));
        }
        if let Some(existing) = state.values.get(&key) {
            if existing != &resolved {
                return Err(ResolveError::Api {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    git_ref: git_ref.to_string(),
                    message: "concurrent resolution returned two different commits".to_string(),
                });
            }
            return Ok(existing.clone());
        }
        state.values.insert(key, resolved.clone());
        Ok(resolved)
    }
}

fn state_error(message: &str) -> ResolveError {
    ResolveError::TaskFailed {
        owner: "<run>".to_string(),
        repo: "<resolution>".to_string(),
        git_ref: "<state>".to_string(),
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct ChangingResolver {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl RefResolver for ChangingResolver {
        async fn resolve(
            &self,
            _owner: &str,
            _repo: &str,
            _git_ref: &str,
        ) -> Result<CommitSha, ResolveError> {
            let digit = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                "a"
            } else {
                "b"
            };
            CommitSha::parse(&digit.repeat(40)).map_err(|error| ResolveError::TaskFailed {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                git_ref: "v1".to_string(),
                message: error.to_string(),
            })
        }
    }

    struct StableResolver {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl RefResolver for StableResolver {
        async fn resolve(
            &self,
            _owner: &str,
            _repo: &str,
            _git_ref: &str,
        ) -> Result<CommitSha, ResolveError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            CommitSha::parse(&"a".repeat(40)).map_err(|error| ResolveError::TaskFailed {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                git_ref: "v1".to_string(),
                message: error.to_string(),
            })
        }
    }

    #[tokio::test]
    async fn freeze_rejects_an_alias_that_moved_during_resolution() {
        let resolver = PinnedRefResolver::new(Arc::new(ChangingResolver {
            calls: AtomicUsize::new(0),
        }));
        resolver
            .resolve("owner", "repo", "v1")
            .await
            .expect("initial resolution should succeed");
        let error = resolver
            .freeze()
            .await
            .expect_err("moved alias must fail finalization");
        assert!(error.to_string().contains("changed during resolution"));
    }

    #[tokio::test]
    async fn frozen_resolution_reuses_the_verified_commit_without_network() {
        let inner = Arc::new(StableResolver {
            calls: AtomicUsize::new(0),
        });
        let resolver = PinnedRefResolver::new(inner.clone());
        let initial = resolver
            .resolve("owner", "repo", "v1")
            .await
            .expect("initial resolution should succeed");
        resolver.freeze().await.expect("stable alias should freeze");
        let execution = resolver
            .resolve("owner", "repo", "v1")
            .await
            .expect("frozen execution should reuse the commit");
        assert_eq!(initial, execution);
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }
}
