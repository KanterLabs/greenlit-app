//! Recursive-descent parser over the token stream, building the [`Expr`]
//! tree.
//!
//! Precedence is encoded directly as a chain of grammar tiers (`or` → `and`
//! → `eq` → `rel` → `unary` → `postfix` → `primary`), each binding tighter
//! than the last — exactly the ordering in the Actions runner's
//! [`ExpressionConstants.cs`](https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionConstants.cs):
//! relational binds tighter than equality, which binds tighter than `&&`,
//! which binds tighter than `||`; `!` and index/dereference/call bind
//! tightest of all. A bare literal can never start a
//! `postfix` chain (only a named-value, function call, or parenthesized
//! group can), matching the runner's "deref/index never follows a
//! bare literal, only via grouping" rule — `'abc'.length` fails to parse
//! here for the same structural reason it fails on GitHub, without needing
//! a separate token-adjacency validator.
//!
//! Function-name and named-value-root validation happen here (not in
//! `eval`), because both are checks against a fixed, data-independent
//! registry — see [`crate::error::ParseError::UnrecognizedFunction`] and
//! [`crate::error::ParseError::UnrecognizedNamedValue`] for why that makes
//! them parse-time errors in this crate, matching the runner's
//! validation-before-evaluation architecture in
//! <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionParser.cs>.

use crate::ast::{BinOp, Expr};
use crate::context::ROOT_NAMES;
use crate::error::{MAX_EXPRESSION_DEPTH, ParseError};
use crate::functions::lookup_arity;
use crate::lexer::{Tok, TokenSpan, tokenize};

/// Parses a full expression (already stripped of its `${{ }}` wrapper, if
/// any) into an [`Expr`] tree.
pub(crate) fn parse(source: &str) -> Result<Expr, ParseError> {
    let tokens = tokenize(source)?;
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        let rest: String = parser.tokens[parser.pos..]
            .iter()
            .map(|t| format!("{:?}", t.tok))
            .collect::<Vec<_>>()
            .join(" ");
        return Err(ParseError::TrailingInput { rest });
    }
    // The runner validates depth after parsing the completed expression tree.
    // Parentheses are therefore transparent, while every operand/container
    // node (including a postfix Index node) consumes a level.
    // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionParser.cs
    if expression_depth(&expr) > MAX_EXPRESSION_DEPTH {
        return Err(ParseError::TooDeep);
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<TokenSpan>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|t| &t.tok)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.tokens.get(self.pos).map(|t| t.tok.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: &Tok, expected: &'static str) -> Result<(), ParseError> {
        match self.bump() {
            Some(found) if &found == want => Ok(()),
            Some(found) => Err(ParseError::UnexpectedToken {
                found: format!("{found:?}"),
                expected,
            }),
            None => Err(ParseError::UnexpectedEof { expected }),
        }
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Some(Tok::OrOr)) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = Expr::Binary {
                op: BinOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_eq()?;
        while matches!(self.peek(), Some(Tok::AndAnd)) {
            self.bump();
            let rhs = self.parse_eq()?;
            lhs = Expr::Binary {
                op: BinOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_rel()?;
        loop {
            let op = match self.peek() {
                Some(Tok::EqEq) => BinOp::Eq,
                Some(Tok::NotEq) => BinOp::NotEq,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_rel()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_rel(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => BinOp::Lt,
                Some(Tok::Le) => BinOp::Le,
                Some(Tok::Gt) => BinOp::Gt,
                Some(Tok::Ge) => BinOp::Ge,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if matches!(self.peek(), Some(Tok::Not)) {
            self.bump();
            let inner = self.parse_unary()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let (expr, postfixable) = self.parse_primary()?;
        // The runner's adjacency table allows `.`/`[` only after
        // `)` `]` `*` PropertyName or NamedValue — never directly after a
        // literal). `'abc'.length`/`'abc'[0]`/`5[0]` must not parse; a
        // parenthesized literal `('abc').length` is fine because the
        // *group* `(...)`, not the literal itself, is what the postfix
        // chain attaches to.
        if !postfixable {
            return Ok(expr);
        }
        let mut expr = expr;
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.bump();
                    match self.bump() {
                        Some(Tok::Ident(name)) => {
                            expr = Expr::Index {
                                target: Box::new(expr),
                                index: Box::new(Expr::Str(name)),
                            };
                        }
                        Some(Tok::Star) => {
                            expr = Expr::Wildcard {
                                target: Box::new(expr),
                            };
                        }
                        Some(other) => {
                            return Err(ParseError::UnexpectedToken {
                                found: format!("{other:?}"),
                                expected: "a property name or '*' after '.'",
                            });
                        }
                        None => {
                            return Err(ParseError::UnexpectedEof {
                                expected: "a property name or '*' after '.'",
                            });
                        }
                    }
                }
                Some(Tok::LBracket) => {
                    self.bump();
                    if matches!(self.peek(), Some(Tok::Star)) {
                        self.bump();
                        self.expect(&Tok::RBracket, "']' after '[*'")?;
                        expr = Expr::Wildcard {
                            target: Box::new(expr),
                        };
                    } else {
                        let index_expr = self.parse_or()?;
                        self.expect(&Tok::RBracket, "']' to close '['")?;
                        expr = Expr::Index {
                            target: Box::new(expr),
                            index: Box::new(index_expr),
                        };
                    }
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<(Expr, bool), ParseError> {
        match self.bump() {
            Some(Tok::Null) => Ok((Expr::Null, false)),
            Some(Tok::True) => Ok((Expr::Bool(true), false)),
            Some(Tok::False) => Ok((Expr::Bool(false), false)),
            Some(Tok::Number(n)) => Ok((Expr::Number(n), false)),
            Some(Tok::Str(s)) => Ok((Expr::Str(s), false)),
            Some(Tok::LParen) => {
                let inner = self.parse_or()?;
                self.expect(&Tok::RParen, "')' to close '('")?;
                Ok((inner, true))
            }
            Some(Tok::Ident(name)) => {
                if matches!(self.peek(), Some(Tok::LParen)) {
                    self.bump();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Tok::RParen)) {
                        loop {
                            args.push(self.parse_or()?);
                            if matches!(self.peek(), Some(Tok::Comma)) {
                                self.bump();
                                continue;
                            }
                            break;
                        }
                    }
                    self.expect(&Tok::RParen, "')' to close function call")?;
                    validate_function(&name, args.len())?;
                    Ok((Expr::Call { name, args }, true))
                } else {
                    validate_named_value(&name)?;
                    Ok((Expr::NamedValue(name), true))
                }
            }
            Some(other) => Err(ParseError::UnexpectedToken {
                found: format!("{other:?}"),
                expected: "an expression",
            }),
            None => Err(ParseError::UnexpectedEof {
                expected: "an expression",
            }),
        }
    }
}

fn expression_depth(expr: &Expr) -> u32 {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) | Expr::NamedValue(_) => 1,
        Expr::Call { args, .. } => 1 + args.iter().map(expression_depth).max().unwrap_or_default(),
        Expr::Index { target, index } => 1 + expression_depth(target).max(expression_depth(index)),
        Expr::Binary {
            op: op @ (BinOp::And | BinOp::Or),
            lhs,
            rhs,
        } => {
            // ExpressionParser flattens nested And/Or nodes into one
            // container before CheckMaxDepth walks the completed tree.
            1 + flattened_logical_parameter_depth(lhs, *op)
                .max(flattened_logical_parameter_depth(rhs, *op))
        }
        Expr::Binary { lhs, rhs, .. } => 1 + expression_depth(lhs).max(expression_depth(rhs)),
        Expr::Wildcard { target } | Expr::Not(target) => 1 + expression_depth(target),
    }
}

fn flattened_logical_parameter_depth(expr: &Expr, flattened_op: BinOp) -> u32 {
    match expr {
        Expr::Binary { op, lhs, rhs } if *op == flattened_op => {
            flattened_logical_parameter_depth(lhs, flattened_op)
                .max(flattened_logical_parameter_depth(rhs, flattened_op))
        }
        _ => expression_depth(expr),
    }
}

fn validate_function(name: &str, given: usize) -> Result<(), ParseError> {
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

fn validate_named_value(name: &str) -> Result<(), ParseError> {
    if ROOT_NAMES
        .iter()
        .any(|root| root.eq_ignore_ascii_case(name))
    {
        Ok(())
    } else {
        Err(ParseError::UnrecognizedNamedValue(name.to_string()))
    }
}
