//! Regenerates expression source text from a folded/residual [`Expr`] tree
//! for display (`Condition::residual_text`) — see the parent module's doc
//! comment for the scope of trees this targets (this module's own output,
//! not a fully general round-trip printer for arbitrary hand-built trees).

use greenlit_expr::value::{format_g15, to_display_string};
use greenlit_expr::{BinOp, Expr, Value};

/// Converts an already-folded [`Value`] back into an [`Expr`] literal, for
/// splicing a resolved operand into a residual tree. Array/Object values
/// have no literal syntax in the expression grammar at all (they only ever
/// arise from context data or `fromJSON`); since `residual` is a plan-time
/// *display* artifact only — Phase 2 execution re-evaluates the original
/// source text fresh against a full runtime context, never this tree —
/// falling back to the same `Array`/`Object` stringification `ToString`
/// already uses elsewhere is a documented, safe simplification.
pub(crate) fn value_to_literal_expr(v: &Value) -> Expr {
    match v {
        Value::Null => Expr::Null,
        Value::Bool(b) => Expr::Bool(*b),
        Value::Number(n) => Expr::Number(*n),
        Value::String(s) => Expr::Str(s.clone()),
        Value::Array(_) | Value::Object(_) => Expr::Str(to_display_string(v)),
    }
}

/// Regenerates expression source text from a folded/residual [`Expr`] tree.
pub(crate) fn pretty_print(expr: &Expr) -> String {
    print_expr(expr, 0)
}

fn print_expr(expr: &Expr, min_prec: u8) -> String {
    match expr {
        Expr::Null => "null".to_string(),
        Expr::Bool(b) => b.to_string(),
        Expr::Number(n) => format_number_literal(*n),
        Expr::Str(s) => format!("'{}'", s.replace('\'', "''")),
        Expr::NamedValue(name) => name.clone(),
        Expr::Call { name, args } => format!(
            "{name}({})",
            args.iter()
                .map(|a| print_expr(a, 0))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Index { target, index } => match index.as_ref() {
            Expr::Str(s) if is_plain_identifier(s) => format!("{}.{}", print_expr(target, 8), s),
            _ => format!("{}[{}]", print_expr(target, 8), print_expr(index, 0)),
        },
        Expr::Wildcard { target } => format!("{}.*", print_expr(target, 8)),
        Expr::Not(inner) => format!("!{}", print_expr(inner, 7)),
        Expr::Binary { op, lhs, rhs } => {
            let (sym, prec) = binop_symbol_and_precedence(*op);
            let text = format!(
                "{} {sym} {}",
                print_expr(lhs, prec),
                print_expr(rhs, prec + 1)
            );
            if prec < min_prec {
                format!("({text})")
            } else {
                text
            }
        }
    }
}

fn binop_symbol_and_precedence(op: BinOp) -> (&'static str, u8) {
    match op {
        BinOp::Or => ("||", 1),
        BinOp::And => ("&&", 2),
        BinOp::Eq => ("==", 3),
        BinOp::NotEq => ("!=", 3),
        BinOp::Lt => ("<", 4),
        BinOp::Le => ("<=", 4),
        BinOp::Gt => (">", 4),
        BinOp::Ge => (">=", 4),
    }
}

fn is_plain_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn format_number_literal(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    format_g15(n)
}
