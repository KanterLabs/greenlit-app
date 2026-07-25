//! Machine-persistent action ref resolutions for exact offline replay.

use std::sync::Arc;

use async_trait::async_trait;
use greenlit_store::cas::CasStore;

use super::{RefResolver, ResolveError};
use crate::CommitSha;

/// Rechecks mutable refs online and records their commit, or resolves only a
/// previously recorded commit while offline.
pub struct PersistentRefResolver {
    inner: Arc<dyn RefResolver>,
    store: CasStore,
    offline: bool,
}

impl PersistentRefResolver {
    /// Wraps `inner` with persistent alias storage.
    #[must_use]
    pub fn new(inner: Arc<dyn RefResolver>, store: CasStore, offline: bool) -> Self {
        Self {
            inner,
            store,
            offline,
        }
    }
}

#[async_trait]
impl RefResolver for PersistentRefResolver {
    async fn resolve(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<CommitSha, ResolveError> {
        let key = format!("{owner}/{repo}@{git_ref}");
        if self.offline {
            let store = self.store.clone();
            let key_for_task = key.clone();
            let stored = tokio::task::spawn_blocking(move || {
                store.resolve_text_alias("action-ref", &key_for_task)
            })
            .await
            .map_err(|error| task_error(owner, repo, git_ref, error.to_string()))?
            .map_err(|error| task_error(owner, repo, git_ref, error.to_string()))?;
            let Some(stored) = stored else {
                return Err(ResolveError::OfflineMissing {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    git_ref: git_ref.to_string(),
                });
            };
            return CommitSha::parse(&stored)
                .map_err(|error| task_error(owner, repo, git_ref, error.to_string()));
        }

        let resolved = self.inner.resolve(owner, repo, git_ref).await?;
        let store = self.store.clone();
        let resolved_text = resolved.as_str().to_string();
        tokio::task::spawn_blocking(move || {
            store.record_text_alias("action-ref", &key, &resolved_text)
        })
        .await
        .map_err(|error| task_error(owner, repo, git_ref, error.to_string()))?
        .map_err(|error| task_error(owner, repo, git_ref, error.to_string()))?;
        Ok(resolved)
    }
}

fn task_error(owner: &str, repo: &str, git_ref: &str, message: String) -> ResolveError {
    ResolveError::TaskFailed {
        owner: owner.to_string(),
        repo: repo.to_string(),
        git_ref: git_ref.to_string(),
        message,
    }
}
