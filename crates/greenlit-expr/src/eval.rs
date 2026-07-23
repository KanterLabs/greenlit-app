//! The evaluator: walks an [`Expr`] tree against a [`Context`], producing a
//! [`Value`].
//!
//! GitHub's public expression contract is documented at
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions>;
//! implementation-only behavior is cited directly to `actions/runner`.
//! `evaluate` is defensive against a *hand-built* [`Expr`] (not only one
//! produced by [`crate::parse`]) — see the [`crate::error::EvalError`]
//! module doc comment for why re-checking function/named-value validity
//! here isn't redundant with the parser's own checks.

use crate::ast::{BinOp, Expr};
use crate::context::Context;
use crate::error::EvalError;
use crate::memory::{EvaluationMemory, ResultMemory, wrapped_primitive_bytes};
use crate::value::{
    Value, abstract_equal, abstract_not_equal, greater_or_equal, greater_than, is_falsy, is_truthy,
    less_or_equal, less_than,
};

mod access;
mod calls;

use access::{index_into, wildcard_filter};
use calls::eval_call;

/// The Actions expression SDK's default maximum live-result memory, in
/// bytes. A caller that supplies no explicit `EvaluationOptions.MaxMemory`
/// receives this 1 MiB limit.
///
/// <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/EvaluationContext.cs#L25-L36>
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 1_048_576;

/// The expression limit used by the runner's workflow-template evaluator.
/// Workflow expressions inherit the template's 10 MiB result budget rather
/// than the bare expression SDK's 1 MiB default.
///
/// <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTPipelines/Pipelines/ObjectTemplating/PipelineTemplateEvaluator.cs#L50-L52>
/// <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTObjectTemplating/ObjectTemplating/Tokens/TemplateToken.cs#L52-L65>
pub const WORKFLOW_TEMPLATE_MAX_MEMORY_BYTES: usize = 10 * 1024 * 1024;

/// Per-evaluation resource options matching the Actions expression SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvaluationOptions {
    max_memory_bytes: usize,
}

impl EvaluationOptions {
    /// Creates options with an explicit live-result memory limit. Zero uses
    /// [`DEFAULT_MAX_MEMORY_BYTES`], matching the runner's sentinel value.
    #[must_use]
    pub const fn new(max_memory_bytes: usize) -> Self {
        Self { max_memory_bytes }
    }

    /// Returns the configured limit; zero means the default 1 MiB limit.
    #[must_use]
    pub const fn max_memory_bytes(self) -> usize {
        self.max_memory_bytes
    }

    const fn effective_max_memory_bytes(self) -> usize {
        if self.max_memory_bytes == 0 {
            DEFAULT_MAX_MEMORY_BYTES
        } else {
            self.max_memory_bytes
        }
    }
}

impl Default for EvaluationOptions {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Evaluates `expr` against `ctx`.
///
/// Parsed expressions have already passed GitHub's depth-50 check. Because
/// [`Expr`] is also public and can be hand-built, this entry point repeats a
/// stack-safe structural check before entering the recursive evaluator.
pub fn evaluate(expr: &Expr, ctx: &Context) -> Result<Value, EvalError> {
    evaluate_with_options(expr, ctx, EvaluationOptions::default())
}

/// Evaluates `expr` with an explicit runner-compatible result-memory budget.
///
/// Use [`WORKFLOW_TEMPLATE_MAX_MEMORY_BYTES`] for the runner's workflow
/// template call path; standalone evaluation uses the 1 MiB SDK default via
/// [`evaluate`].
pub fn evaluate_with_options(
    expr: &Expr,
    ctx: &Context,
    options: EvaluationOptions,
) -> Result<Value, EvalError> {
    if crate::parser::depth::exceeds_safe_evaluation_depth(expr) {
        return Err(EvalError::ExpressionTooDeep);
    }
    let mut memory = EvaluationMemory::new(options.effective_max_memory_bytes());
    evaluate_inner(expr, ctx, &mut memory, 0)
}

fn evaluate_inner(
    expr: &Expr,
    ctx: &Context,
    memory: &mut EvaluationMemory,
    depth: usize,
) -> Result<Value, EvalError> {
    let mut result_memory = ResultMemory::default();
    let value = match expr {
        Expr::Null => Ok(Value::Null),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Number(n) => Ok(Value::Number(*n)),
        Expr::Str(s) => Ok(Value::String(s.clone())),
        Expr::NamedValue(name) => match ctx.resolve_root(name) {
            Some(value) => {
                result_memory.bytes = wrapped_primitive_bytes(value, memory.max_bytes())?;
                result_memory.has_raw = result_memory.bytes.is_some();
                Ok(value.clone())
            }
            None => Err(EvalError::UnrecognizedNamedValue(name.clone())),
        },
        Expr::Not(inner) => {
            let v = evaluate_inner(inner, ctx, memory, depth + 1)?;
            // `!` returns a genuine Boolean by applying the runner's
            // falsy conversion.
            Ok(Value::Bool(is_falsy(&v)))
        }
        Expr::Binary { op, lhs, rhs } => eval_binary(*op, lhs, rhs, ctx, memory, depth),
        Expr::Index { target, index } => {
            let t = evaluate_inner(target, ctx, memory, depth + 1)?;
            // The runner returns null before constructing IndexHelper for a
            // primitive target, so the index expression is never evaluated.
            // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Operators/Index.cs
            if matches!(
                t,
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
            ) {
                Ok(Value::Null)
            } else {
                let i = evaluate_inner(index, ctx, memory, depth + 1)?;
                let (result, bytes, has_raw) = index_into(&t, &i, memory.max_bytes())?;
                result_memory.bytes = bytes;
                result_memory.has_raw = has_raw;
                Ok(result.unwrap_or(Value::Null))
            }
        }
        Expr::Wildcard { target } => {
            let t = evaluate_inner(target, ctx, memory, depth + 1)?;
            // In the runner, `*` is its own child node and is evaluated only
            // after Index confirms the target is a collection. Our AST folds
            // that node into `Expr::Wildcard`, so add the same child-result
            // accounting explicitly for collection targets.
            if matches!(t, Value::Array(_) | Value::Object(_)) {
                memory.add_result(
                    depth + 1,
                    &Value::String("*".to_string()),
                    ResultMemory::default(),
                )?;
            }
            let (result, bytes) = wildcard_filter(&t, memory.max_bytes())?;
            result_memory.bytes = bytes;
            Ok(result)
        }
        Expr::Call { name, args } => {
            let result = eval_call(name, args, ctx, memory, depth)?;
            if name.eq_ignore_ascii_case("fromjson") {
                result_memory.bytes = wrapped_primitive_bytes(&result, memory.max_bytes())?;
                result_memory.has_raw = result_memory.bytes.is_some();
            }
            Ok(result)
        }
    }?;
    memory.add_result(depth, &value, result_memory)?;
    Ok(value)
}

/// `&&`/`||` short-circuit and return an operand's *value* (not coerced to
/// boolean); relational/equality operators always produce a genuine
/// `Boolean`. See GitHub's expression operators documentation:
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#operators>.
fn eval_binary(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    ctx: &Context,
    memory: &mut EvaluationMemory,
    parent_depth: usize,
) -> Result<Value, EvalError> {
    let depth = parent_depth + 1;
    match op {
        BinOp::And => {
            let l = evaluate_inner(lhs, ctx, memory, depth)?;
            // "evaluates operands left to right, returns the value of the
            // first falsy operand, else the last operand's value."
            if is_falsy(&l) {
                Ok(l)
            } else {
                evaluate_inner(rhs, ctx, memory, depth)
            }
        }
        BinOp::Or => {
            let l = evaluate_inner(lhs, ctx, memory, depth)?;
            // "returns the first truthy operand's value, else the last."
            if is_truthy(&l) {
                Ok(l)
            } else {
                evaluate_inner(rhs, ctx, memory, depth)
            }
        }
        BinOp::Eq => {
            let (l, r) = (
                evaluate_inner(lhs, ctx, memory, depth)?,
                evaluate_inner(rhs, ctx, memory, depth)?,
            );
            Ok(Value::Bool(abstract_equal(&l, &r)))
        }
        // "`!=` is defined as exactly `!(==)`."
        BinOp::NotEq => {
            let (l, r) = (
                evaluate_inner(lhs, ctx, memory, depth)?,
                evaluate_inner(rhs, ctx, memory, depth)?,
            );
            Ok(Value::Bool(abstract_not_equal(&l, &r)))
        }
        BinOp::Lt => {
            let (l, r) = (
                evaluate_inner(lhs, ctx, memory, depth)?,
                evaluate_inner(rhs, ctx, memory, depth)?,
            );
            Ok(Value::Bool(less_than(&l, &r)))
        }
        // "`<=` ... including re-running coercions" — `less_or_equal`
        // itself calls both `abstract_equal` and `less_than`, each doing
        // its own coercion pass; evaluating `lhs`/`rhs` only once here and
        // reusing the two `Value`s is just avoiding re-*evaluating the
        // expressions themselves* (which would risk double side effects
        // for something like `hashFiles(...)`), not skipping the
        // documented double coercion.
        BinOp::Le => {
            let (l, r) = (
                evaluate_inner(lhs, ctx, memory, depth)?,
                evaluate_inner(rhs, ctx, memory, depth)?,
            );
            Ok(Value::Bool(less_or_equal(&l, &r)))
        }
        BinOp::Gt => {
            let (l, r) = (
                evaluate_inner(lhs, ctx, memory, depth)?,
                evaluate_inner(rhs, ctx, memory, depth)?,
            );
            Ok(Value::Bool(greater_than(&l, &r)))
        }
        BinOp::Ge => {
            let (l, r) = (
                evaluate_inner(lhs, ctx, memory, depth)?,
                evaluate_inner(rhs, ctx, memory, depth)?,
            );
            Ok(Value::Bool(greater_or_equal(&l, &r)))
        }
    }
}
