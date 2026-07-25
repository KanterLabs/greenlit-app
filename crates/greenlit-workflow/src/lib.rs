#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! `greenlit-workflow`: YAML workflow parsing into a typed workflow model.
//!
//! Parses `.github/workflows/*.yml` into a typed representation covering
//! triggers, jobs, matrices, containers, services, and steps; and exposes a
//! static-extraction API (referenced `secrets.*`/`vars.*` names, authored
//! `needs` output paths, `uses:` references, `runs-on` values) consumed by
//! `greenlit-engine` during planning. See `PHASE-1-engine-core.md` for the
//! full task list this crate implements.
//!
//! # Entry points
//! - [`parse_workflow`] / [`parse_workflow_file`] — parse a workflow's YAML
//!   source (or a path on disk) into a [`model::workflow::Workflow`].
//! - [`extract::extract_static`] — scan an already-parsed [`model::workflow::Workflow`]
//!   for every statically-discoverable `secrets.*`/`vars.*` reference,
//!   statically identifiable `needs.<job>.outputs.<name>` path, `uses:`
//!   reference, and `runs-on` value.
//!
//! Every node in the returned model carries a [`span::Span`] (file, line,
//! column) so callers can render precise error/prompt locations.
//!
//! # Deliberate v0 scope decisions
//!
//! These are flagged here rather than silently guessed at, per `AGENTS.md`
//! "Every conflict found gets flagged in the phase summary" — each is a
//! judgment call made where `PHASE-1-engine-core.md`'s literal field list
//! and real-world GitHub Actions syntax diverge slightly:
//!
//! - **Unknown-key policy**: every mapping-shaped construct is validated
//!   against a fixed allow-list of the keys this crate models (see
//!   `parse::util` module docs); any other key — including real GitHub
//!   keys this phase does not model — is a hard [`error::ParseError::UnknownKey`].
//! - **Job-level `name:`** is modeled even though it is not in
//!   `PHASE-1-engine-core.md`'s literal per-job field list, since omitting
//!   a basic display-name string would make nearly every realistically-named
//!   job fail to parse; it is a zero-risk single-field addition, not new
//!   capability.
//! - **Workflow/job `concurrency:`** is modeled in shorthand and mapping
//!   forms so the scheduler can enforce group ownership.
//! - **Reusable workflow call jobs** (`jobs.<id>.uses:` in place of
//!   `steps:`) are recognized (their `name`/`needs`/`if` are still parsed)
//!   and flagged via [`model::job::Job::reusable_call`], but not deeply
//!   parsed — reusable workflows are out of v0 scope
//!   (`greenlit-v0-spec.md` "Out (v0)").
//! - **Static extraction** (`extract` module) parses each `${{ }}` body
//!   with `greenlit-expr`'s real grammar and walks the resulting AST for
//!   `secrets.*`/`vars.*` and `needs` output references (see `extract`
//!   module docs) — an
//!   earlier interim version of this module did its own best-effort
//!   raw-text scan instead, written before `greenlit-expr` exposed a usable
//!   public lexer/parser entry point; that reconciliation is now done.

pub mod error;
mod expression;
pub mod extract;
pub mod model;
mod parse;
mod span;
mod validate;
mod yaml;

pub use error::ParseError;
pub use extract::{NeedsOutputReference, StaticExtraction, extract_static};
pub use parse::{
    MAX_WORKFLOW_SOURCE_CHARACTERS, parse_workflow, parse_workflow_file,
    parse_workflow_file_with_name,
};
pub use span::{Location, Span, Spanned};
