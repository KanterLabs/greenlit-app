//! Tokenless ref resolution via `git ls-remote`.
//!
//! `PHASE-3-actions.md`: "…or via `git ls-remote` tokenless." Used when no
//! GitHub token is available; works against any public repository without
//! credentials. Process-spawning conventions (bounded stdout/stderr, a hard
//! deadline, no interactive prompts) mirror
//! `crates/greenlit-engine/src/git.rs`'s — see [`crate::gitproc`] for the
//! shared runner.

use std::time::Duration;

use async_trait::async_trait;
use tracing::Instrument;

use crate::gitproc::{self, GitProcessError};
use crate::resolve::{RefResolver, ResolveError};
use crate::sha::CommitSha;
use crate::stage_span;

/// A ref is resolved by asking the remote to advertise the exact,
/// fully-qualified refs it could be; a network round trip is expected to
/// take longer than the local-only commands `greenlit-engine/src/git.rs`
/// bounds at 5 seconds, so this deadline is generous instead.
const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(20);

/// Resolves refs with an unauthenticated `git ls-remote` against
/// `https://github.com/<owner>/<repo>.git` (or, for tests, a caller-supplied
/// base so a local bare repository can stand in for GitHub without any
/// network access).
#[derive(Debug, Clone)]
pub struct GitLsRemoteResolver {
    base_url: String,
}

impl GitLsRemoteResolver {
    /// A resolver that queries real GitHub over HTTPS.
    #[must_use]
    pub fn new() -> Self {
        Self {
            base_url: "https://github.com".to_owned(),
        }
    }

    /// A resolver against `<base>/<owner>/<repo>.git` instead of GitHub —
    /// for tests, `base` is a local directory containing bare repositories
    /// laid out the same way, so real `git` still runs, just against a
    /// filesystem path rather than the network (`TESTING.md`: this is not
    /// mocking `git`, it is giving the true external a local stand-in, the
    /// same way `crates/greenlit-app/tests/support/mod.rs`'s `Sandbox` runs
    /// real `git` against a real local repository rather than faking it).
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Default for GitLsRemoteResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RefResolver for GitLsRemoteResolver {
    async fn resolve(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<CommitSha, ResolveError> {
        let url = format!("{}/{owner}/{repo}.git", self.base_url);
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        let git_ref = git_ref.to_owned();
        let span = stage_span("action-resolve");
        async move {
            let (o, r, g) = (owner.clone(), repo.clone(), git_ref.clone());
            tokio::task::spawn_blocking(move || resolve_blocking(&url, &o, &r, &g))
                .await
                .map_err(|join_error| ResolveError::TaskFailed {
                    owner,
                    repo,
                    git_ref,
                    message: join_error.to_string(),
                })?
        }
        .instrument(span)
        .await
    }
}

fn resolve_blocking(
    url: &str,
    owner: &str,
    repo: &str,
    git_ref: &str,
) -> Result<CommitSha, ResolveError> {
    let head_pattern = format!("refs/heads/{git_ref}");
    let tag_pattern = format!("refs/tags/{git_ref}");
    // Unlike an unfiltered `git ls-remote`, passing explicit ref patterns
    // does *not* implicitly also return an annotated tag's peeled (`^{}`)
    // counterpart — that pattern must be requested explicitly too, or an
    // annotated tag's *own* (tag-object) SHA is all that comes back.
    let peeled_tag_pattern = format!("{tag_pattern}^{{}}");
    let args = [
        "ls-remote",
        url,
        &head_pattern,
        &tag_pattern,
        &peeled_tag_pattern,
    ];
    let output = gitproc::run_git(None, &args, LS_REMOTE_TIMEOUT)
        .map_err(|error| map_process_error(error, owner, repo, git_ref))?;

    if !output.status.success() {
        // A non-existent/private repository and a repository with no
        // matching ref both surface as a non-zero `git ls-remote` exit; see
        // `ResolveError::NotFound`'s doc comment for why this crate does not
        // try to tell them apart.
        return Err(ResolveError::NotFound {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            git_ref: git_ref.to_owned(),
        });
    }
    if output.stdout_truncated {
        // The two ref patterns queried should only ever produce a handful
        // of short lines; a truncated capture means this crate cannot trust
        // that a real match wasn't cut off, so it must not silently return
        // an unmatched (or wrong) result.
        return Err(ResolveError::CommandFailed {
            args: args.join(" "),
            message: format!(
                "git ls-remote output exceeded the {}-byte safety limit",
                gitproc::MAX_GIT_STDOUT_BYTES
            ),
        });
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut head_sha: Option<&str> = None;
    let mut peeled_tag_sha: Option<&str> = None;
    let mut tag_sha: Option<&str> = None;
    for line in text.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        if name == head_pattern {
            head_sha = Some(sha);
        } else if name == peeled_tag_pattern {
            // The peeled (`^{}`) entry for an annotated tag is the *commit*
            // the tag points at; the un-peeled entry would be the tag
            // object's own SHA. GitHub's own tag-resolution behavior always
            // means "the commit", so the peeled SHA is preferred when both
            // are advertised.
            // https://git-scm.com/docs/git-ls-remote
            peeled_tag_sha = Some(sha);
        } else if name == tag_pattern {
            tag_sha = Some(sha);
        }
    }

    // Branches take precedence over tags for an unqualified ref name,
    // matching `actions/checkout`'s own ref resolution order (its
    // `getCheckoutInfo` tries `origin/<ref>` as a branch before falling back
    // to a tag):
    // https://github.com/actions/checkout/blob/main/src/ref-helper.ts
    let resolved = head_sha.or(peeled_tag_sha).or(tag_sha);
    match resolved.and_then(|sha| CommitSha::parse(sha).ok()) {
        Some(sha) => Ok(sha),
        None => Err(ResolveError::NotFound {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            git_ref: git_ref.to_owned(),
        }),
    }
}

fn map_process_error(
    error: GitProcessError,
    owner: &str,
    repo: &str,
    git_ref: &str,
) -> ResolveError {
    match error {
        GitProcessError::TimedOut { seconds, .. } => ResolveError::TimedOut {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            git_ref: git_ref.to_owned(),
            seconds,
        },
        GitProcessError::Spawn { args, message } | GitProcessError::Io { args, message } => {
            ResolveError::CommandFailed { args, message }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A bare repository under `root` with one commit tagged both
    /// lightweight and annotated, plus a second branch — enough surface to
    /// exercise branch/tag precedence and peeling without any network
    /// access (`TESTING.md`: real `git`, a local stand-in remote).
    struct FakeRemote {
        root: tempfile::TempDir,
    }

    impl FakeRemote {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            let work = tempfile::tempdir().expect("tempdir");
            let bare = root.path().join("owner").join("repo.git");
            std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
            run(work.path(), &["init", "-q", "-b", "main"]);
            run(work.path(), &["config", "user.email", "test@example.com"]);
            run(work.path(), &["config", "user.name", "test"]);
            run(work.path(), &["commit", "-q", "--allow-empty", "-m", "one"]);
            run(work.path(), &["tag", "lightweight-tag"]);
            run(
                work.path(),
                &["tag", "-a", "annotated-tag", "-m", "annotated"],
            );
            run(work.path(), &["checkout", "-q", "-b", "feature"]);
            run(work.path(), &["commit", "-q", "--allow-empty", "-m", "two"]);
            run(
                work.path(),
                &[
                    "clone",
                    "-q",
                    "--bare",
                    work.path().to_str().unwrap(),
                    bare.to_str().unwrap(),
                ],
            );
            FakeRemote { root }
        }

        fn resolver(&self) -> GitLsRemoteResolver {
            GitLsRemoteResolver::with_base_url(self.root.path().to_str().unwrap().to_owned())
        }
    }

    fn run(cwd: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn resolves_a_branch() {
        let remote = FakeRemote::new();
        let sha = resolve_ref(&remote, "main").await.unwrap();
        assert_eq!(sha.as_str().len(), 40);
    }

    #[tokio::test]
    async fn resolves_a_lightweight_tag() {
        let remote = FakeRemote::new();
        let sha = resolve_ref(&remote, "lightweight-tag").await.unwrap();
        assert_eq!(sha.as_str().len(), 40);
    }

    #[tokio::test]
    async fn resolves_an_annotated_tag_to_its_peeled_commit() {
        let remote = FakeRemote::new();
        let tag_sha = resolve_ref(&remote, "annotated-tag").await.unwrap();
        let branch_sha = resolve_ref(&remote, "main").await.unwrap();
        // The peeled commit for the annotated tag is the same commit `main`
        // pointed at when the tag was made, not the tag object's own SHA.
        assert_eq!(tag_sha, branch_sha);
    }

    #[tokio::test]
    async fn branch_takes_precedence_over_a_same_named_tag() {
        let remote = FakeRemote::new();
        let bare = remote.root.path().join("owner").join("repo.git");
        // Ambiguously name a tag "feature" pointing at `main`'s commit — a
        // different commit than the `feature` branch's own tip — so the
        // precedence assertion below cannot pass by accident.
        run(&bare, &["tag", "feature", "refs/heads/main"]);

        let branch_tip = first_sha(&gitproc_ls_remote(&remote, "refs/heads/feature"));
        let tag_tip = first_sha(&gitproc_ls_remote(&remote, "refs/tags/feature"));
        assert_ne!(branch_tip, tag_tip, "test setup must make these ambiguous");

        let resolved = resolve_ref(&remote, "feature").await.unwrap();
        assert_eq!(resolved.as_str(), branch_tip);
    }

    fn first_sha(output: &gitproc::GitProcessOutput) -> String {
        std::str::from_utf8(&output.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .expect("at least one ref line")
            .to_owned()
    }

    #[tokio::test]
    async fn missing_ref_is_not_found() {
        let remote = FakeRemote::new();
        let error = resolve_ref(&remote, "does-not-exist").await.unwrap_err();
        assert_eq!(
            error,
            ResolveError::NotFound {
                owner: "owner".into(),
                repo: "repo".into(),
                git_ref: "does-not-exist".into(),
            }
        );
    }

    #[tokio::test]
    async fn missing_repository_is_not_found() {
        let root = tempfile::tempdir().unwrap();
        let resolver = GitLsRemoteResolver::with_base_url(root.path().to_str().unwrap());
        let error = resolver
            .resolve("owner", "does-not-exist", "main")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            ResolveError::NotFound {
                owner: "owner".into(),
                repo: "does-not-exist".into(),
                git_ref: "main".into(),
            }
        );
    }

    async fn resolve_ref(remote: &FakeRemote, git_ref: &str) -> Result<CommitSha, ResolveError> {
        remote.resolver().resolve("owner", "repo", git_ref).await
    }

    fn gitproc_ls_remote(remote: &FakeRemote, pattern: &str) -> gitproc::GitProcessOutput {
        let url = remote
            .root
            .path()
            .join("owner")
            .join("repo.git")
            .to_str()
            .unwrap()
            .to_owned();
        gitproc::run_git(
            None,
            &["ls-remote", &url, pattern],
            std::time::Duration::from_secs(5),
        )
        .unwrap()
    }
}
