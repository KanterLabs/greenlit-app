//! Error types for lexing/parsing and evaluation.
//!
//! Split in two, matching the two fallible public entry points
//! ([`crate::parse`] and [`crate::eval::evaluate`]): a syntax error can be
//! reported without ever touching a [`crate::context::Context`], while an
//! evaluation error can only occur after a successful parse. GitHub's
//! expression SDK likewise separates parse-time errors (`ExceededMaxLength`,
//! `ExceededMaxDepth`, `UnrecognizedNamedValue` — actually validated
//! post-parse, see note on [`EvalError::UnrecognizedNamedValue`]) from
//! evaluation-time errors (`fromJSON` parse failure, `hashFiles` I/O
//! failure, `format` scan failure).

use crate::functions::format::FormatError;
use crate::functions::hash_files::HashFilesError;
use crate::functions::json::FromJsonError;

/// The maximum expression length GitHub accepts, in UTF-16 code units.
///
/// Source: `ExpressionConstants.cs` defines `MaxLength = 21000`, and
/// `ExpressionParser.ParseContext` compares that limit with C#
/// `String.Length`, which counts UTF-16 code units:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionConstants.cs>
/// and
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionParser.cs>.
pub const MAX_EXPRESSION_LENGTH: usize = 21_000;

/// The maximum expression parse-tree depth GitHub accepts.
///
/// Source: `Expressions2/ExpressionConstants.cs` `MaxDepth`, same
/// provenance note as [`MAX_EXPRESSION_LENGTH`].
pub const MAX_EXPRESSION_DEPTH: u32 = 50;

/// A lexing or parsing failure. Never produced once past [`crate::parse`]'s
/// return.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    /// The raw expression source exceeded [`MAX_EXPRESSION_LENGTH`] UTF-16
    /// code units.
    #[error(
        "expression is {actual} UTF-16 code units, exceeding the maximum of {MAX_EXPRESSION_LENGTH}"
    )]
    TooLong {
        /// The expression's actual length in UTF-16 code units.
        actual: usize,
    },
    /// A single-quoted string literal was never closed before the input
    /// ended. Per the docs: strings are single-quoted only, and `''` is the
    /// only escape.
    #[error("unterminated string literal starting at character offset {offset}")]
    UnterminatedString {
        /// Character offset of the opening `'`.
        offset: usize,
    },
    /// A number-shaped token (started with a digit, `+`, `-`, or `.`) failed
    /// to parse under any of decimal/hex/octal/`Infinity` forms.
    #[error("{text:?} is not a valid number literal (character offset {offset})")]
    InvalidNumber {
        /// The raw candidate text the lexer scanned.
        text: String,
        /// Character offset where the candidate started.
        offset: usize,
    },
    /// A lone `=`, `&`, or `|` (not part of a two-character operator).
    #[error("unexpected character {ch:?} at character offset {offset}")]
    UnexpectedChar {
        /// The offending character.
        ch: char,
        /// Character offset it appeared at.
        offset: usize,
    },
    /// The token stream ended where a specific grammar production still
    /// expected more input (e.g. `(1 +` — well, `+` isn't an operator here,
    /// but `1 ==` expecting a right-hand side).
    #[error("unexpected end of expression, expected {expected}")]
    UnexpectedEof {
        /// Human-readable description of what was expected next.
        expected: &'static str,
    },
    /// A token appeared where the grammar did not allow it — this is also
    /// how the "deref/index never follows a bare literal" and "`*` only
    /// after `[` or `.`" adjacency rules surface (see the design memo's
    /// lexical-structure section): the recursive-descent grammar shape
    /// itself rejects these positions, so no separate adjacency-table
    /// validator is needed.
    #[error("unexpected token {found:?}, expected {expected}")]
    UnexpectedToken {
        /// Debug-formatted text of the unexpected token.
        found: String,
        /// Human-readable description of what was expected instead.
        expected: &'static str,
    },
    /// The parse tree exceeded [`MAX_EXPRESSION_DEPTH`].
    #[error("expression nesting exceeds the maximum depth of {MAX_EXPRESSION_DEPTH}")]
    TooDeep,
    /// A built-in function was called with an argument count outside its
    /// registered arity. Arities are fixed per function (see
    /// `functions::registered_arity`); wrong arity is a parse-time error in
    /// GitHub's SDK (`ExpressionConstants.cs` function registration), not an
    /// evaluation-time one, which is why it lives here.
    #[error("{name}() does not accept {given} argument(s) (expected {expected})")]
    WrongArity {
        /// The called function's name.
        name: String,
        /// How many arguments were actually given.
        given: usize,
        /// Human-readable accepted arity (e.g. `"2"`, `"1-255"`).
        expected: &'static str,
    },
    /// `case()` was given an even argument count instead of predicate/value
    /// pairs followed by one default value.
    #[error("case() requires an odd number of arguments")]
    EvenCaseParameters,
    /// The expression called a function name that isn't one of the built-ins
    /// this crate registers. Real GitHub validates function names against a
    /// case-insensitive registry independent of any context *data* (only
    /// which workflow key the expression lives in restricts which of
    /// `hashFiles`/the status functions are legal; `greenlit-workflow`
    /// applies that per-key policy after this syntax parser succeeds. This
    /// remains a parse-time registry error because it needs no context data.
    #[error("'{0}' is not a recognized function")]
    UnrecognizedFunction(String),
    /// The expression referenced a context root name outside the fixed set
    /// this crate recognizes (`github`, `env`, `vars`, `secrets`, `needs`,
    /// `matrix`, `strategy`, `steps`, `runner`, `job`, `inputs`). Contrast
    /// with an unrecognized *property* on a recognized root, which evaluates
    /// to an empty string (see "About contexts" in the Contexts
    /// reference: "If you attempt to dereference a nonexistent property, it
    /// will evaluate to an empty string" — that rule is about properties,
    /// not roots). Real GitHub additionally restricts *which* of these eleven
    /// roots are legal per workflow key (the "Context availability" table);
    /// `greenlit-workflow` enforces that site-specific policy because this
    /// standalone parser intentionally has no workflow-key location.
    #[error("'{0}' is not a recognized named-value (context) in this expression")]
    UnrecognizedNamedValue(String),
    /// Extra input remained after a complete expression was parsed.
    #[error("unexpected trailing input: {rest:?}")]
    TrailingInput {
        /// Debug-formatted text of the leftover tokens.
        rest: String,
    },
}

/// An evaluation-time failure. Everything else in the language degrades to
/// a documented missing value rather than erroring (see `eval::index_into`) — these are the
/// only genuine runtime errors GitHub's expression evaluator raises.
///
/// [`EvalError::UnrecognizedFunction`] and
/// [`EvalError::UnrecognizedNamedValue`] duplicate checks
/// [`ParseError::UnrecognizedFunction`]/[`ParseError::UnrecognizedNamedValue`]
/// already perform against the same fixed name registries. That is
/// deliberate, not redundant: [`crate::ast::Expr`] is a public AST that a
/// caller can build directly (not only via [`crate::parse`]), so
/// [`crate::eval::evaluate`] cannot assume its input already passed parser
/// validation — it re-checks and reports its own error rather than
/// panicking or silently misbehaving on a hand-built tree.
///
/// No `PartialEq` here (unlike [`ParseError`]): [`HashFilesError`] wraps
/// `std::io::Error`, which does not implement it. Tests match on the
/// specific variant/fields they care about instead.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// See [`ParseError::UnrecognizedFunction`] — same check, re-applied
    /// defensively at evaluation time.
    #[error("'{0}' is not a recognized function")]
    UnrecognizedFunction(String),
    /// See [`ParseError::UnrecognizedNamedValue`] — same check, re-applied
    /// defensively at evaluation time.
    #[error("'{0}' is not a recognized named-value (context) in this expression")]
    UnrecognizedNamedValue(String),
    /// See [`ParseError::WrongArity`] — re-applied defensively at
    /// evaluation time. A hand-built [`crate::ast::Expr::Call`] with too
    /// few arguments would otherwise panic when a built-in function
    /// indexes into its argument list; checking here first keeps
    /// `evaluate` panic-free for any input, not only parser output.
    #[error("{name}() does not accept {given} argument(s) (expected {expected})")]
    WrongArity {
        /// The called function's name.
        name: String,
        /// How many arguments were actually given.
        given: usize,
        /// Human-readable accepted arity (e.g. `"2"`, `"1-255"`).
        expected: &'static str,
    },
    /// `case()` requires predicate/value pairs followed by one default.
    /// Parser-produced trees are checked earlier; this evaluation variant
    /// keeps hand-built public ASTs panic-free.
    #[error("case() requires predicate/value pairs followed by one default value")]
    InvalidCaseArity,
    /// A `case()` predicate produced a non-Boolean value. The runner checks
    /// the concrete value kind rather than applying truthiness coercion.
    #[error("case() predicate {position} evaluated to {kind:?}, expected Boolean")]
    InvalidCasePredicate {
        /// One-based predicate position within the call.
        position: usize,
        /// The predicate's actual runtime kind.
        kind: crate::value::ValueKind,
    },
    /// A `format()` call error — see [`FormatError`].
    #[error(transparent)]
    Format(#[from] FormatError),
    /// A `fromJSON()` call error — see [`FromJsonError`].
    #[error(transparent)]
    FromJson(#[from] FromJsonError),
    /// A `hashFiles()` call error — see [`HashFilesError`].
    #[error(transparent)]
    HashFiles(#[from] HashFilesError),
}
