#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! `greenlit-actions`: marketplace-action support for `litci run`.
//!
//! Phase 3 (`PHASE-3-actions.md`) fills this crate in: `uses:` reference
//! parsing (`owner/repo@ref`, subdir form, `./local`, `docker://`),
//! ref-to-SHA resolution (GitHub API with a token, `git ls-remote`
//! tokenless), fetching into the content-addressed store under
//! `~/.litci/actions/<owner>/<repo>/<sha>/`, and `action.yml` parsing
//! (inputs/outputs/runs including pre/post and their conditions).
