//! Explicit-frame expression parsing; no source nesting uses the Rust stack.

use crate::ast::{BinOp, Expr};
use crate::error::ParseError;
use crate::lexer::{Tok, TokenSpan};

use super::depth::ensure_expression_depth;
use super::{validate_function, validate_named_value};

mod state;
use state::{ExpressionState, Operand};

pub(super) fn parse_tokens(tokens: Vec<TokenSpan>) -> Result<Expr, ParseError> {
    let mut frames = vec![Frame::new(FrameKind::Root)];
    let mut pos = 0usize;

    while let Some(token) = tokens.get(pos).map(|token| token.tok.clone()) {
        match token {
            Tok::Null => push_literal(&mut frames, Expr::Null)?,
            Tok::True => push_literal(&mut frames, Expr::Bool(true))?,
            Tok::False => push_literal(&mut frames, Expr::Bool(false))?,
            Tok::Number(number) => push_literal(&mut frames, Expr::Number(number))?,
            Tok::Str(value) => push_literal(&mut frames, Expr::Str(value))?,
            Tok::Ident(name) => {
                ensure_operand_position(&frames, &Tok::Ident(name.clone()))?;
                if matches!(
                    tokens.get(pos + 1).map(|token| &token.tok),
                    Some(Tok::LParen)
                ) {
                    frames.push(Frame::new(FrameKind::Function {
                        name,
                        args: Vec::new(),
                    }));
                    pos += 2;
                    continue;
                }
                validate_named_value(&name)?;
                current_frame(&mut frames)?
                    .state
                    .push_operand(Operand::atom(Expr::NamedValue(name), true))?;
            }
            Tok::Not => current_frame(&mut frames)?.state.push_not()?,
            Tok::EqEq => push_binary(&mut frames, BinOp::Eq)?,
            Tok::NotEq => push_binary(&mut frames, BinOp::NotEq)?,
            Tok::Lt => push_binary(&mut frames, BinOp::Lt)?,
            Tok::Le => push_binary(&mut frames, BinOp::Le)?,
            Tok::Gt => push_binary(&mut frames, BinOp::Gt)?,
            Tok::Ge => push_binary(&mut frames, BinOp::Ge)?,
            Tok::AndAnd => push_binary(&mut frames, BinOp::And)?,
            Tok::OrOr => push_binary(&mut frames, BinOp::Or)?,
            Tok::LParen => {
                ensure_operand_position(&frames, &Tok::LParen)?;
                frames.push(Frame::new(FrameKind::Group));
            }
            Tok::RParen => close_parenthesis(&mut frames)?,
            Tok::LBracket => {
                let target = current_frame(&mut frames)?.state.take_postfix_target()?;
                if matches!(tokens.get(pos + 1).map(|token| &token.tok), Some(Tok::Star)) {
                    match tokens.get(pos + 2).map(|token| &token.tok) {
                        Some(Tok::RBracket) => {
                            let expr = Expr::Wildcard {
                                target: Box::new(target),
                            };
                            ensure_expression_depth(&expr)?;
                            current_frame(&mut frames)?.state.put_postfix(expr);
                            pos += 3;
                            continue;
                        }
                        _ => {
                            return Err(ParseError::UnexpectedToken {
                                found: "Star".to_string(),
                                expected: "']' after '[*'",
                            });
                        }
                    }
                }
                frames.push(Frame::new(FrameKind::Index { target }));
            }
            Tok::RBracket => close_index(&mut frames)?,
            Tok::Dot => {
                let target = current_frame(&mut frames)?.state.take_postfix_target()?;
                let next = tokens.get(pos + 1).map(|token| token.tok.clone());
                let expr = match next {
                    Some(Tok::Ident(name)) => Expr::Index {
                        target: Box::new(target),
                        index: Box::new(Expr::Str(name)),
                    },
                    Some(Tok::Star) => Expr::Wildcard {
                        target: Box::new(target),
                    },
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
                };
                ensure_expression_depth(&expr)?;
                current_frame(&mut frames)?.state.put_postfix(expr);
                pos += 2;
                continue;
            }
            Tok::Comma => finish_function_argument(&mut frames)?,
            Tok::Star => {
                return Err(unexpected(&Tok::Star, "an expression"));
            }
        }
        pos += 1;
    }

    if frames.len() != 1 {
        let expected = match frames.last().map(|frame| &frame.kind) {
            Some(FrameKind::Group | FrameKind::Function { .. }) => "')' to close '('",
            Some(FrameKind::Index { .. }) => "']' to close '['",
            Some(FrameKind::Root) | None => "an expression",
        };
        return Err(ParseError::UnexpectedEof { expected });
    }
    let frame = frames.pop().ok_or(ParseError::UnexpectedEof {
        expected: "an expression",
    })?;
    frame.state.finish()?.into_expr()
}

fn push_literal(frames: &mut [Frame], expr: Expr) -> Result<(), ParseError> {
    current_frame(frames)?
        .state
        .push_operand(Operand::atom(expr, false))
}

fn push_binary(frames: &mut [Frame], op: BinOp) -> Result<(), ParseError> {
    current_frame(frames)?.state.push_binary(op)
}

fn ensure_operand_position(frames: &[Frame], found: &Tok) -> Result<(), ParseError> {
    match frames.last() {
        Some(frame) if frame.state.expects_operand() => Ok(()),
        Some(_) => Err(unexpected(found, "an operator")),
        None => Err(ParseError::UnexpectedEof {
            expected: "an expression",
        }),
    }
}

fn close_parenthesis(frames: &mut Vec<Frame>) -> Result<(), ParseError> {
    let closable = matches!(
        frames.last().map(|frame| &frame.kind),
        Some(FrameKind::Group | FrameKind::Function { .. })
    );
    if !closable {
        return Err(unexpected(&Tok::RParen, "an operator"));
    }
    let frame = frames.pop().ok_or(ParseError::UnexpectedEof {
        expected: "an expression",
    })?;
    match frame.kind {
        FrameKind::Group => {
            let value = frame.state.finish()?;
            current_frame(frames)?
                .state
                .push_operand(Operand::new(value, true))
        }
        FrameKind::Function { name, mut args } => {
            if frame.state.started() {
                args.push(frame.state.finish()?.into_expr()?);
            } else if !args.is_empty() {
                return Err(ParseError::UnexpectedToken {
                    found: "RParen".to_string(),
                    expected: "an expression after ','",
                });
            }
            validate_function(&name, args.len())?;
            let expr = Expr::Call { name, args };
            ensure_expression_depth(&expr)?;
            current_frame(frames)?
                .state
                .push_operand(Operand::atom(expr, true))
        }
        FrameKind::Root | FrameKind::Index { .. } => Err(unexpected(&Tok::RParen, "an operator")),
    }
}

fn close_index(frames: &mut Vec<Frame>) -> Result<(), ParseError> {
    if !matches!(
        frames.last().map(|frame| &frame.kind),
        Some(FrameKind::Index { .. })
    ) {
        return Err(unexpected(&Tok::RBracket, "an operator"));
    }
    let frame = frames.pop().ok_or(ParseError::UnexpectedEof {
        expected: "an expression",
    })?;
    let FrameKind::Index { target } = frame.kind else {
        return Err(unexpected(&Tok::RBracket, "an index expression"));
    };
    let index = frame.state.finish()?.into_expr()?;
    let expr = Expr::Index {
        target: Box::new(target),
        index: Box::new(index),
    };
    ensure_expression_depth(&expr)?;
    current_frame(frames)?.state.put_postfix(expr);
    Ok(())
}

fn finish_function_argument(frames: &mut [Frame]) -> Result<(), ParseError> {
    let frame = current_frame(frames)?;
    let FrameKind::Function { args, .. } = &mut frame.kind else {
        return Err(unexpected(&Tok::Comma, "an operator"));
    };
    let state = std::mem::take(&mut frame.state);
    args.push(state.finish()?.into_expr()?);
    Ok(())
}

fn current_frame(frames: &mut [Frame]) -> Result<&mut Frame, ParseError> {
    frames.last_mut().ok_or(ParseError::UnexpectedEof {
        expected: "an expression",
    })
}

fn unexpected(found: &Tok, expected: &'static str) -> ParseError {
    ParseError::UnexpectedToken {
        found: format!("{found:?}"),
        expected,
    }
}

struct Frame {
    kind: FrameKind,
    state: ExpressionState,
}

impl Frame {
    fn new(kind: FrameKind) -> Self {
        Self {
            kind,
            state: ExpressionState::default(),
        }
    }
}

enum FrameKind {
    Root,
    Group,
    Function { name: String, args: Vec<Expr> },
    Index { target: Expr },
}
