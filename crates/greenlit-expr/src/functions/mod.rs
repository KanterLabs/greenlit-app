//! Built-in functions.
//!
//! This module implements GitHub's complete built-in set, including the
//! currently documented `case()` function. The Phase 1 brief's explicit
//! list predates `case()` and conflicts with the product specification's
//! complete-function requirement; product behavior takes precedence under
//! `AGENTS.md`.
//!
//! Arities are fixed constants (design memo §3, "Arities … from
//! `ExpressionConstants.cs` registration"), independent of any evaluated
//! data, which is why `lookup_arity` is consulted by the *parser* (a
//! wrong-arity call is a [`crate::error::ParseError`], not an
//! [`crate::error::EvalError`]).

pub(crate) mod affixes;
pub(crate) mod contains;
pub mod format;
pub mod hash_files;
pub(crate) mod join;
pub mod json;

/// An accepted argument-count range for a registered function, plus the
/// human-readable form used in [`crate::error::ParseError::WrongArity`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Arity {
    pub min: usize,
    pub max: usize,
    pub display: &'static str,
}

/// Looks up the registered arity for `name`, matched case-insensitively
/// (e.g. `fromJson`/`fromJSON`/`FROMJSON` all resolve). Returns `None` for
/// any name outside the fixed built-in set.
pub(crate) fn lookup_arity(name: &str) -> Option<Arity> {
    let lower = name.to_ascii_lowercase();
    let a = |min, max, display| Some(Arity { min, max, display });
    match lower.as_str() {
        "contains" => a(2, 2, "2"),
        "startswith" => a(2, 2, "2"),
        "endswith" => a(2, 2, "2"),
        "format" => a(1, 255, "1-255"),
        "join" => a(1, 2, "1-2"),
        "tojson" => a(1, 1, "1"),
        "fromjson" => a(1, 1, "1"),
        "hashfiles" => a(1, 255, "1-255"),
        // Registration deliberately permits even counts; ExpressionParser
        // performs the odd-count syntax check and Case.cs repeats it
        // defensively during evaluation.
        // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionConstants.cs
        "case" => a(3, 255, "3-255"),
        "success" => a(0, 0, "0"),
        "failure" => a(0, 0, "0"),
        "cancelled" => a(0, 0, "0"),
        "always" => a(0, 0, "0"),
        _ => None,
    }
}
