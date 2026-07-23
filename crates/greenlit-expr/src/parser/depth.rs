//! Stack-safe Actions-compatible AST depth accounting.

use crate::ast::{BinOp, Expr};
use crate::error::{MAX_EXPRESSION_DEPTH, ParseError};

/// Checks GitHub's semantic expression-tree depth without recursively
/// walking a potentially attacker-shaped Rust tree. The runner flattens
/// adjacent `&&` or adjacent `||` nodes into a single container before its
/// `CheckMaxDepth` walk, so same-operator logical children do not consume an
/// additional level here either.
/// https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/ExpressionParser.cs
pub(crate) fn ensure_expression_depth(expr: &Expr) -> Result<(), ParseError> {
    let mut stack = vec![(expr, 1u32)];
    while let Some((node, node_depth)) = stack.pop() {
        if node_depth > MAX_EXPRESSION_DEPTH {
            return Err(ParseError::TooDeep);
        }
        match node {
            Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) | Expr::NamedValue(_) => {}
            Expr::Call { args, .. } => {
                stack.extend(args.iter().map(|arg| (arg, node_depth + 1)));
            }
            Expr::Index { target, index } => {
                stack.push((target, node_depth + 1));
                stack.push((index, node_depth + 1));
            }
            Expr::Wildcard { target } | Expr::Not(target) => {
                stack.push((target, node_depth + 1));
            }
            Expr::Binary {
                op: op @ (BinOp::And | BinOp::Or),
                lhs,
                rhs,
            } => {
                push_logical_child(&mut stack, lhs, *op, node_depth);
                push_logical_child(&mut stack, rhs, *op, node_depth);
            }
            Expr::Binary { lhs, rhs, .. } => {
                stack.push((lhs, node_depth + 1));
                stack.push((rhs, node_depth + 1));
            }
        }
    }
    Ok(())
}

/// Reports whether a caller-built AST exceeds the recursion depth the
/// evaluator can safely walk. Unlike parser depth accounting, this counts
/// the concrete Rust representation of every node: parser-produced logical
/// chains are balanced, while a public caller could hand-build a pathological
/// left spine of same-operator [`Expr::Binary`] nodes.
pub(crate) fn exceeds_safe_evaluation_depth(expr: &Expr) -> bool {
    let mut stack = vec![(expr, 1u32)];
    while let Some((node, node_depth)) = stack.pop() {
        if node_depth > MAX_EXPRESSION_DEPTH {
            return true;
        }
        match node {
            Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) | Expr::NamedValue(_) => {}
            Expr::Call { args, .. } => {
                stack.extend(args.iter().map(|arg| (arg, node_depth + 1)));
            }
            Expr::Index { target, index } => {
                stack.push((target, node_depth + 1));
                stack.push((index, node_depth + 1));
            }
            Expr::Wildcard { target } | Expr::Not(target) => {
                stack.push((target, node_depth + 1));
            }
            Expr::Binary { lhs, rhs, .. } => {
                stack.push((lhs, node_depth + 1));
                stack.push((rhs, node_depth + 1));
            }
        }
    }
    false
}

fn push_logical_child<'a>(
    stack: &mut Vec<(&'a Expr, u32)>,
    child: &'a Expr,
    parent_op: BinOp,
    parent_depth: u32,
) {
    let child_depth = match child {
        Expr::Binary { op, .. } if *op == parent_op => parent_depth,
        _ => parent_depth + 1,
    };
    stack.push((child, child_depth));
}

/// Builds an equivalent balanced tree for an associative logical chain.
/// Evaluation remains left-to-right and short-circuiting, while balancing
/// prevents Rust's recursive `Drop`/evaluation from seeing a thousands-node
/// spine. GitHub models the same authored chain as one flat container.
pub(super) fn build_balanced_logical(
    mut operands: Vec<Expr>,
    op: BinOp,
) -> Result<Expr, ParseError> {
    while operands.len() > 1 {
        let mut next = Vec::with_capacity(operands.len().div_ceil(2));
        let mut iter = operands.into_iter();
        while let Some(lhs) = iter.next() {
            match iter.next() {
                Some(rhs) => next.push(Expr::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }),
                None => next.push(lhs),
            }
        }
        operands = next;
    }
    let expr = operands
        .into_iter()
        .next()
        .ok_or(ParseError::UnexpectedEof {
            expected: "an expression",
        })?;
    ensure_expression_depth(&expr)?;
    Ok(expr)
}
