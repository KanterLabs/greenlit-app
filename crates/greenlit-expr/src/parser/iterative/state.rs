//! One explicit parser frame's operand/operator state.

use crate::ast::{BinOp, Expr};
use crate::error::ParseError;

use super::super::depth::{build_balanced_logical, ensure_expression_depth};

pub(super) struct ExpressionState {
    operands: Vec<Operand>,
    operators: Vec<Operator>,
    expecting_operand: bool,
    started: bool,
}

impl Default for ExpressionState {
    fn default() -> Self {
        Self {
            operands: Vec::new(),
            operators: Vec::new(),
            expecting_operand: true,
            started: false,
        }
    }
}

impl ExpressionState {
    pub(super) fn expects_operand(&self) -> bool {
        self.expecting_operand
    }

    pub(super) fn started(&self) -> bool {
        self.started
    }

    pub(super) fn push_operand(&mut self, operand: Operand) -> Result<(), ParseError> {
        if !self.expecting_operand {
            return Err(ParseError::UnexpectedToken {
                found: "expression".to_string(),
                expected: "an operator",
            });
        }
        self.operands.push(operand);
        self.expecting_operand = false;
        self.started = true;
        Ok(())
    }

    pub(super) fn push_not(&mut self) -> Result<(), ParseError> {
        if !self.expecting_operand {
            return Err(ParseError::UnexpectedToken {
                found: "Not".to_string(),
                expected: "an operator",
            });
        }
        self.operators.push(Operator::Not);
        self.started = true;
        Ok(())
    }

    pub(super) fn push_binary(&mut self, op: BinOp) -> Result<(), ParseError> {
        if self.expecting_operand {
            return Err(ParseError::UnexpectedToken {
                found: format!("{op:?}"),
                expected: "an expression",
            });
        }
        while self
            .operators
            .last()
            .is_some_and(|top| top.precedence() >= Operator::Binary(op).precedence())
        {
            self.reduce_top()?;
        }
        self.operators.push(Operator::Binary(op));
        self.expecting_operand = true;
        Ok(())
    }

    pub(super) fn take_postfix_target(&mut self) -> Result<Expr, ParseError> {
        if self.expecting_operand {
            return Err(ParseError::UnexpectedToken {
                found: "postfix operator".to_string(),
                expected: "an expression",
            });
        }
        let operand = self.operands.pop().ok_or(ParseError::UnexpectedEof {
            expected: "an expression before a postfix operator",
        })?;
        if !operand.postfixable {
            return Err(ParseError::UnexpectedToken {
                found: "postfix operator".to_string(),
                expected: "a named value, function call, group, or index target",
            });
        }
        operand.value.into_expr()
    }

    pub(super) fn put_postfix(&mut self, expr: Expr) {
        self.operands.push(Operand::atom(expr, true));
        self.expecting_operand = false;
        self.started = true;
    }

    pub(super) fn finish(mut self) -> Result<PendingExpr, ParseError> {
        if !self.started {
            return Err(ParseError::UnexpectedEof {
                expected: "an expression",
            });
        }
        if self.expecting_operand {
            return Err(ParseError::UnexpectedEof {
                expected: "an expression after an operator",
            });
        }
        while !self.operators.is_empty() {
            self.reduce_top()?;
        }
        if self.operands.len() != 1 {
            return Err(ParseError::TrailingInput {
                rest: "multiple expressions".to_string(),
            });
        }
        self.operands
            .pop()
            .map(|operand| operand.value)
            .ok_or(ParseError::UnexpectedEof {
                expected: "an expression",
            })
    }

    fn reduce_top(&mut self) -> Result<(), ParseError> {
        let operator = self.operators.pop().ok_or(ParseError::UnexpectedEof {
            expected: "an operator",
        })?;
        let rhs = self.operands.pop().ok_or(ParseError::UnexpectedEof {
            expected: "a right-hand operand",
        })?;
        let value = match operator {
            Operator::Not => {
                let expr = Expr::Not(Box::new(rhs.value.into_expr()?));
                ensure_expression_depth(&expr)?;
                PendingExpr::Atom(expr)
            }
            Operator::Binary(op) => {
                let lhs = self.operands.pop().ok_or(ParseError::UnexpectedEof {
                    expected: "a left-hand operand",
                })?;
                if matches!(op, BinOp::And | BinOp::Or) {
                    PendingExpr::merge_logical(op, lhs.value, rhs.value)?
                } else {
                    let expr = Expr::Binary {
                        op,
                        lhs: Box::new(lhs.value.into_expr()?),
                        rhs: Box::new(rhs.value.into_expr()?),
                    };
                    ensure_expression_depth(&expr)?;
                    PendingExpr::Atom(expr)
                }
            }
        };
        self.operands.push(Operand::new(value, false));
        Ok(())
    }
}

pub(super) struct Operand {
    value: PendingExpr,
    postfixable: bool,
}

impl Operand {
    pub(super) fn atom(expr: Expr, postfixable: bool) -> Self {
        Self::new(PendingExpr::Atom(expr), postfixable)
    }

    pub(super) fn new(value: PendingExpr, postfixable: bool) -> Self {
        Self { value, postfixable }
    }
}

pub(super) enum PendingExpr {
    Atom(Expr),
    Logical { op: BinOp, operands: Vec<Expr> },
}

impl PendingExpr {
    fn merge_logical(op: BinOp, lhs: Self, rhs: Self) -> Result<Self, ParseError> {
        let mut operands = match lhs {
            PendingExpr::Logical {
                op: lhs_op,
                operands,
            } if lhs_op == op => operands,
            other => vec![other.into_expr()?],
        };
        match rhs {
            PendingExpr::Logical {
                op: rhs_op,
                operands: rhs_operands,
            } if rhs_op == op => operands.extend(rhs_operands),
            other => operands.push(other.into_expr()?),
        }
        Ok(PendingExpr::Logical { op, operands })
    }

    pub(super) fn into_expr(self) -> Result<Expr, ParseError> {
        match self {
            PendingExpr::Atom(expr) => {
                ensure_expression_depth(&expr)?;
                Ok(expr)
            }
            PendingExpr::Logical { op, operands } => build_balanced_logical(operands, op),
        }
    }
}

#[derive(Clone, Copy)]
enum Operator {
    Not,
    Binary(BinOp),
}

impl Operator {
    fn precedence(self) -> u8 {
        match self {
            Operator::Not => 5,
            Operator::Binary(BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) => 4,
            Operator::Binary(BinOp::Eq | BinOp::NotEq) => 3,
            Operator::Binary(BinOp::And) => 2,
            Operator::Binary(BinOp::Or) => 1,
        }
    }
}
