#![forbid(unsafe_code)]

//! `greenlit-expr`: the GitHub Actions `${{ }}` expression language.
//!
//! Lexer, parser, and evaluator for the full expression grammar — literals,
//! operators, property/index access, object filters, built-in functions
//! (`contains`, `startsWith`, `endsWith`, `format`, `join`, `toJSON`,
//! `fromJSON`, `hashFiles`, `case`, and the status functions), GitHub's exact
//! type-coercion and loose-equality rules, and the typed context model
//! (`github`, `env`, `vars`, `secrets`, `needs`, `matrix`, `steps`, `runner`,
//! `job`, `inputs`). See `PHASE-1-engine-core.md` for the full task list this
//! crate implements.
//!
//! # Entry points for other crates
//!
//! - [`parse`] — turns the text *inside* a `${{ … }}` wrapper (the wrapper
//!   itself and any surrounding literal template text are `greenlit-workflow`'s
//!   concern, not this crate's — see the [`ast`] module doc comment) into an
//!   [`ast::Expr`]. `greenlit-workflow` calls this while parsing a workflow
//!   file, to catch a syntax error, an unrecognized function name, or an
//!   unrecognized context root the moment the workflow is parsed — before any
//!   [`context::Context`] exists.
//! - [`eval::evaluate`] — evaluates a parsed [`ast::Expr`] against a
//!   [`context::Context`] a caller (`greenlit-engine`) has built for a
//!   specific job/step, producing a [`value::Value`].
//! - [`ast::expr_calls_status_function`] — lets a caller (`greenlit-engine`)
//!   decide whether an `if:` expression needs the implicit `success() &&`
//!   wrap (see that function's doc comment for the exact rule and why
//!   *applying* the wrap is the caller's job, not this crate's).
//!
//! # A note on `unsafe`
//!
//! This crate has no `unsafe` blocks; `#![forbid(unsafe_code)]` above is a
//! hard compiler-enforced guarantee of that, per `AGENTS.md`'s repo hygiene
//! rule.

pub mod ast;
pub mod context;
pub mod error;
pub mod eval;
pub mod functions;
mod lexer;
mod parser;
pub mod value;

#[cfg(test)]
mod oracle_tests;

pub use ast::{BinOp, Expr, expr_calls_status_function};
pub use context::{Context, RunStatus};
pub use error::{EvalError, ParseError};
pub use eval::evaluate;
pub use functions::format::FormatError;
pub use functions::hash_files::{DirEntry, EntryKind, HashFilesError, HashFilesFs, RealFs};
pub use functions::json::FromJsonError;
pub use value::{ArrayValue, ObjectValue, Value, ValueKind};

/// Parses the text inside a `${{ … }}` wrapper into an [`Expr`] tree.
///
/// The `${{ }}` delimiters themselves must already be stripped by the
/// caller — this function parses exactly the grammar in GitHub's
/// [Expressions reference](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions),
/// nothing about YAML template interpolation. Function names
/// and context-root names are validated here against this crate's fixed
/// registries (see [`error::ParseError::UnrecognizedFunction`] and
/// [`error::ParseError::UnrecognizedNamedValue`]); everything else about an
/// expression's *meaning* (property lookups, coercions, `hashFiles`'
/// filesystem access) is deferred to [`eval::evaluate`].
///
/// # Examples
/// ```
/// use greenlit_expr::parse;
///
/// let expr = parse("github.event_name == 'push'").unwrap();
/// assert!(matches!(expr, greenlit_expr::Expr::Binary { .. }));
/// ```
pub fn parse(source: &str) -> Result<Expr, ParseError> {
    parser::parse(source)
}
