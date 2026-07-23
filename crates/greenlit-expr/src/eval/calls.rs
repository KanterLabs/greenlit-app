//! Built-in function dispatch during expression evaluation.

use crate::ast::Expr;
use crate::context::{Context, RunStatus};
use crate::error::EvalError;
use crate::functions::{self, lookup_arity};
use crate::memory::EvaluationMemory;
use crate::value::{Value, to_display_string};

use super::evaluate_inner;

pub(super) fn eval_call(
    name: &str,
    args: &[Expr],
    ctx: &Context,
    memory: &mut EvaluationMemory,
    parent_depth: usize,
) -> Result<Value, EvalError> {
    let depth = parent_depth + 1;
    let arity =
        lookup_arity(name).ok_or_else(|| EvalError::UnrecognizedFunction(name.to_string()))?;
    if args.len() < arity.min || args.len() > arity.max {
        return Err(EvalError::WrongArity {
            name: name.to_string(),
            given: args.len(),
            expected: arity.display,
        });
    }

    match name.to_ascii_lowercase().as_str() {
        "contains" => {
            let search = evaluate_inner(&args[0], ctx, memory, depth)?;
            // Contains evaluates its second parameter only for a primitive
            // or a non-empty array. Objects and empty arrays return false
            // immediately.
            // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Contains.cs
            if matches!(search, Value::Object(_))
                || matches!(&search, Value::Array(array) if array.is_empty())
            {
                Ok(Value::Bool(false))
            } else {
                let item = evaluate_inner(&args[1], ctx, memory, depth)?;
                Ok(functions::contains::contains(&search, &item))
            }
        }
        "startswith" => {
            let s = evaluate_inner(&args[0], ctx, memory, depth)?;
            if matches!(s, Value::Array(_) | Value::Object(_)) {
                Ok(Value::Bool(false))
            } else {
                let prefix = evaluate_inner(&args[1], ctx, memory, depth)?;
                Ok(functions::affixes::starts_with(&s, &prefix))
            }
        }
        "endswith" => {
            let s = evaluate_inner(&args[0], ctx, memory, depth)?;
            // StartsWith and EndsWith use the same primitive guard as the
            // runner: a collection first argument returns false without
            // evaluating the second parameter.
            // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/EndsWith.cs
            if matches!(s, Value::Array(_) | Value::Object(_)) {
                Ok(Value::Bool(false))
            } else {
                let suffix = evaluate_inner(&args[1], ctx, memory, depth)?;
                Ok(functions::affixes::ends_with(&s, &suffix))
            }
        }
        "format" => {
            let fmt_value = evaluate_inner(&args[0], ctx, memory, depth)?;
            let fmt_str = to_display_string(&fmt_value);
            let value_args = &args[1..];
            let max_memory_bytes = memory.max_bytes();
            let result =
                functions::format::format(&fmt_str, value_args.len(), max_memory_bytes, |i| {
                    evaluate_inner(&value_args[i], ctx, memory, depth)
                })?;
            Ok(result)
        }
        "join" => {
            let array = evaluate_inner(&args[0], ctx, memory, depth)?;
            let max_memory_bytes = memory.max_bytes();
            let separator = args.get(1);
            functions::join::join(
                &array,
                separator.is_some(),
                max_memory_bytes,
                || match separator {
                    Some(expression) => evaluate_inner(expression, ctx, memory, depth),
                    None => Ok(Value::Null),
                },
            )
        }
        "tojson" => {
            let v = evaluate_inner(&args[0], ctx, memory, depth)?;
            functions::json::to_json(&v, memory.max_bytes())
        }
        "fromjson" => {
            let v = evaluate_inner(&args[0], ctx, memory, depth)?;
            let s = to_display_string(&v);
            Ok(functions::json::from_json(&s)?)
        }
        "hashfiles" => {
            let mut strs = Vec::with_capacity(args.len());
            for arg in args {
                strs.push(to_display_string(&evaluate_inner(arg, ctx, memory, depth)?));
            }
            let result =
                functions::hash_files::hash_files(&strs, ctx.fs(), ctx.hash_files_clock())?;
            Ok(Value::String(result))
        }
        "case" => eval_case(args, ctx, memory, parent_depth),
        // Status functions are evaluated against the injected `RunStatus`,
        // matching GitHub's documented status-check functions.
        "success" => Ok(Value::Bool(ctx.status() == RunStatus::Success)),
        "failure" => Ok(Value::Bool(ctx.status() == RunStatus::Failure)),
        "cancelled" => Ok(Value::Bool(ctx.status() == RunStatus::Cancelled)),
        "always" => Ok(Value::Bool(true)),
        _ => Err(EvalError::UnrecognizedFunction(name.to_string())),
    }
}

fn eval_case(
    args: &[Expr],
    ctx: &Context,
    memory: &mut EvaluationMemory,
    parent_depth: usize,
) -> Result<Value, EvalError> {
    let depth = parent_depth + 1;
    // Case.cs requires an odd count and a concrete Boolean predicate. It
    // evaluates predicates in order and only the selected value (or final
    // default), so failed branches remain unevaluated.
    // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Case.cs
    if args.len().is_multiple_of(2) {
        return Err(EvalError::InvalidCaseArity);
    }
    for (pair_index, pair) in args[..args.len() - 1].chunks_exact(2).enumerate() {
        match evaluate_inner(&pair[0], ctx, memory, depth)? {
            Value::Bool(true) => return evaluate_inner(&pair[1], ctx, memory, depth),
            Value::Bool(false) => {}
            other => {
                return Err(EvalError::InvalidCasePredicate {
                    position: pair_index + 1,
                    kind: other.kind(),
                });
            }
        }
    }
    evaluate_inner(&args[args.len() - 1], ctx, memory, depth)
}
