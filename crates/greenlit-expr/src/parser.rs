//! Iterative parser over the expression token stream.
//!
//! Operator precedence follows the Actions runner's
//! [`ExpressionConstants.cs`](https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionConstants.cs).
//! Frames replace recursive calls for grouping, function arguments, and
//! index expressions, so every source accepted by the runner's 21,000-unit
//! length limit is safe to parse regardless of delimiter nesting.

use crate::ast::Expr;
use crate::context::ROOT_NAMES;
use crate::error::ParseError;
use crate::functions::lookup_arity;
use crate::lexer::tokenize;

pub(crate) mod depth;
mod iterative;

/// Parses a full expression (already stripped of its `${{ }}` wrapper, if
/// any) into an [`Expr`] tree.
pub(crate) fn parse(source: &str) -> Result<Expr, ParseError> {
    let tokens = tokenize(source)?;
    let expr = iterative::parse_tokens(tokens)?;
    // GitHub checks semantic AST depth after its own iterative parse.
    // Parentheses remain transparent, while every actual AST container
    // consumes one level.
    // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionParser.cs
    depth::ensure_expression_depth(&expr)?;
    Ok(expr)
}

pub(super) fn validate_function(name: &str, given: usize) -> Result<(), ParseError> {
    match lookup_arity(name) {
        None => Err(ParseError::UnrecognizedFunction(name.to_string())),
        Some(arity) => {
            if given < arity.min || given > arity.max {
                Err(ParseError::WrongArity {
                    name: name.to_string(),
                    given,
                    expected: arity.display,
                })
            } else if name.eq_ignore_ascii_case("case") && given.is_multiple_of(2) {
                // ExpressionParser performs this check when closing the
                // function call, even though ExpressionConstants registers
                // the broader 3..255 range.
                // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionParser.cs
                Err(ParseError::EvenCaseParameters)
            } else {
                Ok(())
            }
        }
    }
}

pub(super) fn validate_named_value(name: &str) -> Result<(), ParseError> {
    if ROOT_NAMES
        .iter()
        .any(|root| root.eq_ignore_ascii_case(name))
    {
        Ok(())
    } else {
        Err(ParseError::UnrecognizedNamedValue(name.to_string()))
    }
}
