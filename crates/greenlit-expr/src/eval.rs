//! The evaluator: walks an [`Expr`] tree against a [`Context`], producing a
//! [`Value`].
//!
//! Every rule implemented here cites the specific design-memo section (in
//! turn cited to GitHub's docs or `actions/runner` source) it reproduces.
//! `evaluate` is defensive against a *hand-built* [`Expr`] (not only one
//! produced by [`crate::parse`]) — see the [`crate::error::EvalError`]
//! module doc comment for why re-checking function/named-value validity
//! here isn't redundant with the parser's own checks.

use crate::ast::{BinOp, Expr};
use crate::context::Context;
use crate::error::EvalError;
use crate::functions::{self, lookup_arity};
use crate::value::{
    ArrayValue, Value, abstract_equal, abstract_not_equal, greater_or_equal, greater_than,
    is_falsy, is_truthy, less_or_equal, less_than, to_display_string, to_number,
};

/// Evaluates `expr` against `ctx`.
pub fn evaluate(expr: &Expr, ctx: &Context) -> Result<Value, EvalError> {
    match expr {
        Expr::Null => Ok(Value::Null),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Number(n) => Ok(Value::Number(*n)),
        Expr::Str(s) => Ok(Value::String(s.clone())),
        Expr::NamedValue(name) => ctx
            .resolve_root(name)
            .cloned()
            .ok_or_else(|| EvalError::UnrecognizedNamedValue(name.clone())),
        Expr::Not(inner) => {
            let v = evaluate(inner, ctx)?;
            // "`!` returns a genuine Boolean: `!x` => IsFalsy(x)" (design
            // memo §1.2).
            Ok(Value::Bool(is_falsy(&v)))
        }
        Expr::Binary { op, lhs, rhs } => eval_binary(*op, lhs, rhs, ctx),
        Expr::Index { target, index } => {
            let t = evaluate(target, ctx)?;
            // The runner returns null before constructing IndexHelper for a
            // primitive target, so the index expression is never evaluated.
            // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Operators/Index.cs
            if matches!(
                t,
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
            ) {
                Ok(Value::Null)
            } else {
                let i = evaluate(index, ctx)?;
                Ok(index_into(&t, &i))
            }
        }
        Expr::Wildcard { target } => {
            let t = evaluate(target, ctx)?;
            Ok(wildcard_filter(&t))
        }
        Expr::Call { name, args } => eval_call(name, args, ctx),
    }
}

/// `&&`/`||` short-circuit and return an operand's *value* (not coerced to
/// boolean); relational/equality operators always produce a genuine
/// `Boolean`. Design memo §1.2 ("Semantics of the logical operators") and
/// §2.5/§2.6.
fn eval_binary(op: BinOp, lhs: &Expr, rhs: &Expr, ctx: &Context) -> Result<Value, EvalError> {
    match op {
        BinOp::And => {
            let l = evaluate(lhs, ctx)?;
            // "evaluates operands left to right, returns the value of the
            // first falsy operand, else the last operand's value."
            if is_falsy(&l) {
                Ok(l)
            } else {
                evaluate(rhs, ctx)
            }
        }
        BinOp::Or => {
            let l = evaluate(lhs, ctx)?;
            // "returns the first truthy operand's value, else the last."
            if is_truthy(&l) {
                Ok(l)
            } else {
                evaluate(rhs, ctx)
            }
        }
        BinOp::Eq => {
            let (l, r) = (evaluate(lhs, ctx)?, evaluate(rhs, ctx)?);
            Ok(Value::Bool(abstract_equal(&l, &r)))
        }
        // "`!=` is defined as exactly `!(==)`."
        BinOp::NotEq => {
            let (l, r) = (evaluate(lhs, ctx)?, evaluate(rhs, ctx)?);
            Ok(Value::Bool(abstract_not_equal(&l, &r)))
        }
        BinOp::Lt => {
            let (l, r) = (evaluate(lhs, ctx)?, evaluate(rhs, ctx)?);
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
            let (l, r) = (evaluate(lhs, ctx)?, evaluate(rhs, ctx)?);
            Ok(Value::Bool(less_or_equal(&l, &r)))
        }
        BinOp::Gt => {
            let (l, r) = (evaluate(lhs, ctx)?, evaluate(rhs, ctx)?);
            Ok(Value::Bool(greater_than(&l, &r)))
        }
        BinOp::Ge => {
            let (l, r) = (evaluate(lhs, ctx)?, evaluate(rhs, ctx)?);
            Ok(Value::Bool(greater_or_equal(&l, &r)))
        }
    }
}

/// `target[index]` / `target.property` — design memo §1.4, "nothing here
/// ever throws".
fn index_into(target: &Value, index: &Value) -> Value {
    match target {
        // "Target is not a collection (null or primitive): result is Null."
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Value::Null,
        Value::Object(obj) => {
            // "if its result is a primitive it is converted with ToString
            // ... Missing key or non-primitive index -> Null."
            if matches!(index, Value::Array(_) | Value::Object(_)) {
                return Value::Null;
            }
            let key = to_display_string(index);
            obj.get(&key).cloned().unwrap_or(Value::Null)
        }
        Value::Array(arr) if arr.is_filtered() => index_into_filtered(arr, index),
        Value::Array(arr) => index_into_plain_array(arr, index),
    }
}

/// "Target is an array: index result -> ToNumber; if NaN or `< 0` -> Null;
/// `floor()` it; if `> i32::MAX` -> Null; if `>= len` -> Null; else the
/// element."
fn index_into_plain_array(arr: &ArrayValue, index: &Value) -> Value {
    let n = to_number(index);
    if n.is_nan() || n < 0.0 {
        return Value::Null;
    }
    let floored = n.floor();
    if floored > f64::from(i32::MAX) {
        return Value::Null;
    }
    arr.items()
        .get(floored as usize)
        .cloned()
        .unwrap_or(Value::Null)
}

/// Indexing a *filtered* array (design memo §1.4): "for each item that is
/// an object and has the key (case-insensitive), append its value; items
/// lacking the key are silently skipped; non-object items skipped" for a
/// string-shaped index, and "for each item that is an array with that
/// index in range, append that element" for an integer-shaped one.
/// Per-element dispatch (object items tried as a key lookup, array items
/// tried as a numeric lookup) rather than picking one mode for the whole
/// call reproduces this correctly even when a filtered array holds a mix
/// of object and array elements, using the single evaluated index value's
/// two coercions (`ToString` for object items, `ToNumber` for array items).
fn index_into_filtered(arr: &ArrayValue, index: &Value) -> Value {
    if matches!(index, Value::Array(_) | Value::Object(_)) {
        // Not documented explicitly for filtered arrays; consistent with
        // every other "non-primitive index" case in this function, which
        // is always Null/empty rather than an error.
        return Value::filtered_array(vec![]);
    }
    let as_key = to_display_string(index);
    let as_number = to_number(index);
    let mut out = Vec::new();
    for item in arr.items() {
        match item {
            Value::Object(o) => {
                if let Some(v) = o.get(&as_key) {
                    out.push(v.clone());
                }
            }
            Value::Array(a) => {
                if as_number.is_nan() || as_number < 0.0 {
                    continue;
                }
                let floored = as_number.floor();
                if floored > f64::from(i32::MAX) {
                    continue;
                }
                if let Some(v) = a.items().get(floored as usize) {
                    out.push(v.clone());
                }
            }
            _ => {}
        }
    }
    Value::filtered_array(out)
}

/// `target.*` / `target[*]` — design memo §1.4.
fn wildcard_filter(target: &Value) -> Value {
    match target {
        // "Filtered arrays ... `.*`: flatten one level — for each item
        // that is an object append all its values, for each array item
        // append all elements, scalar items skipped."
        Value::Array(arr) if arr.is_filtered() => {
            let mut out = Vec::new();
            for item in arr.items() {
                match item {
                    Value::Object(o) => out.extend(o.iter().map(|(_, v)| v.clone())),
                    Value::Array(a) => out.extend(a.items().iter().cloned()),
                    _ => {}
                }
            }
            Value::filtered_array(out)
        }
        // "Wildcard `*` on an array: filtered array of all elements in
        // order."
        Value::Array(arr) => Value::filtered_array(arr.items().to_vec()),
        // "On an object: filtered array of all values in the object's
        // stored order."
        Value::Object(obj) => Value::filtered_array(obj.iter().map(|(_, v)| v.clone()).collect()),
        // "unless the index is the wildcard `*`, in which case the result
        // is an empty filtered array" (the null-or-primitive-target case).
        _ => Value::filtered_array(vec![]),
    }
}

fn eval_call(name: &str, args: &[Expr], ctx: &Context) -> Result<Value, EvalError> {
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
            let search = evaluate(&args[0], ctx)?;
            // Contains evaluates its second parameter only for a primitive
            // or a non-empty array. Objects and empty arrays return false
            // immediately.
            // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Contains.cs
            if matches!(search, Value::Object(_))
                || matches!(&search, Value::Array(array) if array.is_empty())
            {
                Ok(Value::Bool(false))
            } else {
                let item = evaluate(&args[1], ctx)?;
                Ok(functions::contains::contains(&search, &item))
            }
        }
        "startswith" => {
            let s = evaluate(&args[0], ctx)?;
            if matches!(s, Value::Array(_) | Value::Object(_)) {
                Ok(Value::Bool(false))
            } else {
                let prefix = evaluate(&args[1], ctx)?;
                Ok(functions::affixes::starts_with(&s, &prefix))
            }
        }
        "endswith" => {
            let s = evaluate(&args[0], ctx)?;
            // StartsWith and EndsWith use the same primitive guard as the
            // runner: a collection first argument returns false without
            // evaluating the second parameter.
            // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/EndsWith.cs
            if matches!(s, Value::Array(_) | Value::Object(_)) {
                Ok(Value::Bool(false))
            } else {
                let suffix = evaluate(&args[1], ctx)?;
                Ok(functions::affixes::ends_with(&s, &suffix))
            }
        }
        "format" => {
            let fmt_value = evaluate(&args[0], ctx)?;
            let fmt_str = to_display_string(&fmt_value);
            let value_args = &args[1..];
            let result = functions::format::format(&fmt_str, value_args.len(), |i| {
                evaluate(&value_args[i], ctx)
            })?;
            Ok(result)
        }
        "join" => {
            let array = evaluate(&args[0], ctx)?;
            // "Separator is only evaluated if the array has >= 2 elements."
            let needs_separator =
                matches!(&array, Value::Array(a) if a.len() >= 2) && args.len() > 1;
            let sep = if needs_separator {
                Some(evaluate(&args[1], ctx)?)
            } else {
                None
            };
            Ok(functions::join::join(&array, sep.as_ref()))
        }
        "tojson" => {
            let v = evaluate(&args[0], ctx)?;
            Ok(functions::json::to_json(&v))
        }
        "fromjson" => {
            let v = evaluate(&args[0], ctx)?;
            let s = to_display_string(&v);
            Ok(functions::json::from_json(&s)?)
        }
        "hashfiles" => {
            let mut strs = Vec::with_capacity(args.len());
            for a in args {
                strs.push(to_display_string(&evaluate(a, ctx)?));
            }
            Ok(functions::hash_files::hash_files(&strs, ctx.fs())?)
        }
        "case" => eval_case(args, ctx),
        // Status functions (design memo §4): evaluated against the
        // injected `RunStatus`, never erroring.
        "success" => Ok(Value::Bool(
            ctx.status() == crate::context::RunStatus::Success,
        )),
        "failure" => Ok(Value::Bool(
            ctx.status() == crate::context::RunStatus::Failure,
        )),
        "cancelled" => Ok(Value::Bool(
            ctx.status() == crate::context::RunStatus::Cancelled,
        )),
        "always" => Ok(Value::Bool(true)),
        _ => Err(EvalError::UnrecognizedFunction(name.to_string())),
    }
}

fn eval_case(args: &[Expr], ctx: &Context) -> Result<Value, EvalError> {
    // Case.cs requires an odd count and a concrete Boolean predicate. It
    // evaluates predicates in order and only the selected value (or final
    // default), so failed branches remain unevaluated.
    // https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/Case.cs
    if args.len().is_multiple_of(2) {
        return Err(EvalError::InvalidCaseArity);
    }
    for (pair_index, pair) in args[..args.len() - 1].chunks_exact(2).enumerate() {
        match evaluate(&pair[0], ctx)? {
            Value::Bool(true) => return evaluate(&pair[1], ctx),
            Value::Bool(false) => {}
            other => {
                return Err(EvalError::InvalidCasePredicate {
                    position: pair_index + 1,
                    kind: other.kind(),
                });
            }
        }
    }
    evaluate(&args[args.len() - 1], ctx)
}
