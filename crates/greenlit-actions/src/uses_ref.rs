//! Parsing a step's `uses:` value into a typed [`ActionRef`].
//!
//! GitHub documents exactly four forms
//! (<https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/find-and-customize-actions>):
//! `{owner}/{repo}@{ref}` and `{owner}/{repo}/{path}@{ref}` (an action in
//! another repository, optionally in a subdirectory), `./path/to/dir` (an
//! action in the *same* repository, relative to the default working
//! directory), and `docker://{image}:{tag}` (a published Docker image).
//! [`ActionRef::parse`] recognizes exactly these four and rejects everything
//! else with a structured, span-free [`UsesRefError`] — spans are the
//! caller's concern (the `uses:` string already came from a
//! [`greenlit_workflow::Spanned`] value the caller holds).

use std::fmt;

/// A parsed `uses:` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRef {
    /// `{owner}/{repo}@{ref}` or `{owner}/{repo}/{path}@{ref}` — an action
    /// published in a (possibly different) GitHub repository.
    Repository(RepositoryRef),
    /// `./path/to/dir` — an action in the same repository as the workflow,
    /// read directly from the already-checked-out workspace rather than
    /// fetched into the action store (see [`crate::store`] module docs for
    /// why this bypasses the store).
    Local {
        /// The path exactly as authored, including its leading `./`.
        path: String,
    },
    /// `docker://{image}` (with or without a `:{tag}`/`@{digest}`) — a
    /// published Docker image, pulled by the container engine rather than
    /// fetched into the action store (see [`crate::store`] module docs).
    Docker {
        /// The image reference text following `docker://`, verbatim.
        image: String,
    },
}

/// `{owner}/{repo}@{ref}`, optionally with a subdirectory path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRef {
    /// The repository owner (user or organization) login.
    pub owner: String,
    /// The repository name.
    pub repo: String,
    /// The subdirectory within the repository the action lives in, when the
    /// `{owner}/{repo}/{path}@{ref}` form was used (`None` for the plain
    /// `{owner}/{repo}@{ref}` form).
    pub path: Option<String>,
    /// The ref exactly as authored — a tag, branch name, or commit SHA.
    /// [`crate::resolve`] turns this into a [`crate::CommitSha`].
    pub git_ref: String,
}

impl ActionRef {
    /// Parses a step's `uses:` value.
    ///
    /// # Errors
    /// Returns [`UsesRefError`] naming exactly which of the four documented
    /// forms `input` failed to match.
    pub fn parse(input: &str) -> Result<Self, UsesRefError> {
        if input.is_empty() {
            return Err(UsesRefError::Empty);
        }
        if let Some(image) = input.strip_prefix("docker://") {
            return parse_docker(image);
        }
        if input == "." || input.starts_with("./") {
            return parse_local(input);
        }
        parse_repository(input)
    }
}

fn parse_docker(image: &str) -> Result<ActionRef, UsesRefError> {
    if image.is_empty() {
        return Err(UsesRefError::EmptyDockerImage);
    }
    Ok(ActionRef::Docker {
        image: image.to_owned(),
    })
}

/// GitHub documents the local form as relative "(`./`) to the default
/// working directory" of the same repository — never a parent-directory
/// escape. `path` is rejected if any `..` component would walk above the
/// leading `./`, per the host-filesystem invariant (`AGENTS.md`) that
/// repository content is untrusted and a fetched/authored reference must
/// never be able to name a location outside the checkout.
fn parse_local(path: &str) -> Result<ActionRef, UsesRefError> {
    if path == "./" || path == "." {
        return Err(UsesRefError::EmptyLocalPath);
    }
    let mut depth: i64 = 0;
    for component in path.split('/').skip(1) {
        match component {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return Err(UsesRefError::LocalPathEscapesRepository {
                        path: path.to_owned(),
                    });
                }
            }
            _ => depth += 1,
        }
    }
    Ok(ActionRef::Local {
        path: path.to_owned(),
    })
}

fn parse_repository(input: &str) -> Result<ActionRef, UsesRefError> {
    let (path_part, git_ref) = input
        .rsplit_once('@')
        .ok_or_else(|| UsesRefError::MissingRef {
            input: input.to_owned(),
        })?;
    if git_ref.is_empty() {
        return Err(UsesRefError::EmptyRef {
            input: input.to_owned(),
        });
    }
    let mut segments = path_part.split('/');
    let owner = segments.next().unwrap_or_default();
    let repo = segments.next().ok_or_else(|| UsesRefError::MissingRepo {
        input: input.to_owned(),
    })?;
    if owner.is_empty() || repo.is_empty() {
        return Err(UsesRefError::MissingRepo {
            input: input.to_owned(),
        });
    }
    let remaining: Vec<&str> = segments.collect();
    let path = if remaining.is_empty() {
        None
    } else {
        if remaining.iter().any(|segment| segment.is_empty()) {
            return Err(UsesRefError::EmptyPathSegment {
                input: input.to_owned(),
            });
        }
        Some(remaining.join("/"))
    };
    Ok(ActionRef::Repository(RepositoryRef {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        path,
        git_ref: git_ref.to_owned(),
    }))
}

/// `uses:` did not match one of GitHub's four documented forms.
///
/// Deliberately span-free: the workflow value this was parsed from is
/// already a [`greenlit_workflow::Spanned`] the caller holds, so the caller
/// (a later Phase 3 task group wiring this into `greenlit-engine` planning)
/// attaches that span when rendering a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UsesRefError {
    /// The `uses:` value was an empty string.
    #[error("`uses:` value is empty")]
    Empty,
    /// `docker://` was followed by nothing.
    #[error("`uses: docker://` is missing an image reference")]
    EmptyDockerImage,
    /// `./` (or `.`) named no path at all.
    #[error("`uses:` local action path is empty")]
    EmptyLocalPath,
    /// A local `./...` path contains more `..` segments than it has
    /// descended, which would resolve outside the repository root.
    #[error("`uses: {path}` would resolve outside the repository checkout")]
    LocalPathEscapesRepository {
        /// The offending value.
        path: String,
    },
    /// Neither `docker://` nor `./` nor an `{owner}/{repo}@{ref}`-shaped
    /// value with a ref (no `@` at all).
    #[error("`uses: {input}` is missing an `@ref`")]
    MissingRef {
        /// The offending value.
        input: String,
    },
    /// The text before `@` did not contain at least an owner and a repo.
    #[error("`uses: {input}` is missing a repository (expected `owner/repo@ref`)")]
    MissingRepo {
        /// The offending value.
        input: String,
    },
    /// The ref after `@` was empty.
    #[error("`uses: {input}` has an empty ref after `@`")]
    EmptyRef {
        /// The offending value.
        input: String,
    },
    /// A subdirectory path segment (`owner/repo/.../@ref`) was empty, e.g.
    /// a doubled slash.
    #[error("`uses: {input}` has an empty path segment")]
    EmptyPathSegment {
        /// The offending value.
        input: String,
    },
}

impl fmt::Display for ActionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionRef::Repository(r) => {
                write!(f, "{}/{}", r.owner, r.repo)?;
                if let Some(path) = &r.path {
                    write!(f, "/{path}")?;
                }
                write!(f, "@{}", r.git_ref)
            }
            ActionRef::Local { path } => f.write_str(path),
            ActionRef::Docker { image } => write!(f, "docker://{image}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Oracle table: one row per documented `uses:` form plus its rejection
    // edge cases (`greenlit-v0-spec.md`/`PHASE-3-actions.md`: "Parse `uses:`
    // forms: `owner/repo@ref`, `owner/repo/subdir@ref`, `./local/path`,
    // `docker://image:tag`").
    #[test]
    fn parses_owner_repo_ref() {
        let parsed = ActionRef::parse("actions/checkout@v4").unwrap();
        assert_eq!(
            parsed,
            ActionRef::Repository(RepositoryRef {
                owner: "actions".to_owned(),
                repo: "checkout".to_owned(),
                path: None,
                git_ref: "v4".to_owned(),
            })
        );
    }

    #[test]
    fn parses_owner_repo_subdir_ref() {
        let parsed = ActionRef::parse("actions/aws/ec2@main").unwrap();
        assert_eq!(
            parsed,
            ActionRef::Repository(RepositoryRef {
                owner: "actions".to_owned(),
                repo: "aws".to_owned(),
                path: Some("ec2".to_owned()),
                git_ref: "main".to_owned(),
            })
        );
    }

    #[test]
    fn parses_multi_segment_subdir() {
        let parsed = ActionRef::parse("owner/repo/deep/nested/dir@abc").unwrap();
        let ActionRef::Repository(r) = parsed else {
            panic!("expected Repository");
        };
        assert_eq!(r.path.as_deref(), Some("deep/nested/dir"));
    }

    #[test]
    fn parses_full_sha_ref() {
        let sha = "a".repeat(40);
        let parsed = ActionRef::parse(&format!("actions/checkout@{sha}")).unwrap();
        let ActionRef::Repository(r) = parsed else {
            panic!("expected Repository");
        };
        assert_eq!(r.git_ref, sha);
    }

    #[test]
    fn ref_may_contain_slashes_for_branch_names() {
        let parsed = ActionRef::parse("owner/repo@feature/my-branch").unwrap();
        let ActionRef::Repository(r) = parsed else {
            panic!("expected Repository");
        };
        assert_eq!(r.git_ref, "feature/my-branch");
    }

    #[test]
    fn parses_local_path() {
        let parsed = ActionRef::parse("./.github/actions/hello-world-action").unwrap();
        assert_eq!(
            parsed,
            ActionRef::Local {
                path: "./.github/actions/hello-world-action".to_owned()
            }
        );
    }

    #[test]
    fn parses_docker_image_with_tag() {
        let parsed = ActionRef::parse("docker://alpine:3.8").unwrap();
        assert_eq!(
            parsed,
            ActionRef::Docker {
                image: "alpine:3.8".to_owned()
            }
        );
    }

    #[test]
    fn parses_docker_image_without_tag() {
        let parsed = ActionRef::parse("docker://alpine").unwrap();
        assert_eq!(
            parsed,
            ActionRef::Docker {
                image: "alpine".to_owned()
            }
        );
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(ActionRef::parse("").unwrap_err(), UsesRefError::Empty);
    }

    #[test]
    fn rejects_empty_docker_image() {
        assert_eq!(
            ActionRef::parse("docker://").unwrap_err(),
            UsesRefError::EmptyDockerImage
        );
    }

    #[test]
    fn rejects_bare_dot_local_path() {
        assert_eq!(
            ActionRef::parse("./").unwrap_err(),
            UsesRefError::EmptyLocalPath
        );
        assert_eq!(
            ActionRef::parse(".").unwrap_err(),
            UsesRefError::EmptyLocalPath
        );
    }

    #[test]
    fn rejects_local_path_escaping_repository() {
        assert_eq!(
            ActionRef::parse("./../outside").unwrap_err(),
            UsesRefError::LocalPathEscapesRepository {
                path: "./../outside".to_owned()
            }
        );
    }

    #[test]
    fn accepts_local_path_that_descends_before_ascending() {
        // Net depth never goes negative, so this stays inside the checkout.
        assert!(ActionRef::parse("./a/../b").is_ok());
    }

    #[test]
    fn rejects_missing_ref() {
        assert_eq!(
            ActionRef::parse("actions/checkout").unwrap_err(),
            UsesRefError::MissingRef {
                input: "actions/checkout".to_owned()
            }
        );
    }

    #[test]
    fn rejects_missing_repo_segment() {
        assert_eq!(
            ActionRef::parse("actions@v4").unwrap_err(),
            UsesRefError::MissingRepo {
                input: "actions@v4".to_owned()
            }
        );
    }

    #[test]
    fn rejects_empty_owner() {
        assert_eq!(
            ActionRef::parse("/checkout@v4").unwrap_err(),
            UsesRefError::MissingRepo {
                input: "/checkout@v4".to_owned()
            }
        );
    }

    #[test]
    fn rejects_empty_ref() {
        assert_eq!(
            ActionRef::parse("actions/checkout@").unwrap_err(),
            UsesRefError::EmptyRef {
                input: "actions/checkout@".to_owned()
            }
        );
    }

    #[test]
    fn rejects_empty_subdir_segment() {
        assert_eq!(
            ActionRef::parse("owner/repo//sub@ref").unwrap_err(),
            UsesRefError::EmptyPathSegment {
                input: "owner/repo//sub@ref".to_owned()
            }
        );
    }

    #[test]
    fn display_roundtrips_repository_form() {
        let parsed = ActionRef::parse("actions/checkout@v4").unwrap();
        assert_eq!(parsed.to_string(), "actions/checkout@v4");
    }

    #[test]
    fn display_roundtrips_subdir_form() {
        let parsed = ActionRef::parse("actions/aws/ec2@main").unwrap();
        assert_eq!(parsed.to_string(), "actions/aws/ec2@main");
    }
}
