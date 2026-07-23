//! The expression abstract syntax tree.
//!
//! GitHub Actions expressions are a single flat grammar (no statements) — see
//! the [Expressions reference](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions).
//! `a.b` is defined as sugar for `a['b']` (both build the same `Index` node);
//! see [`Expr::Index`].

/// A parsed `${{ }}` expression (the inner grammar only — the `${{ }}`
/// wrapper and any surrounding literal template text are a *template-layer*
/// concern for `greenlit-workflow`, not this crate; see
/// [Expressions: about expressions](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#about-expressions)).
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// The `null` literal keyword.
    Null,
    /// The `true` / `false` literal keywords.
    Bool(bool),
    /// A numeric literal: decimal, hex (`0x…`), octal (`0o…`), or the `NaN`
    /// / `Infinity` / `-Infinity` keyword forms. Always stored as the parsed
    /// `f64` — GitHub Actions has exactly one numeric kind (see
    /// `value::Value::Number`).
    Number(f64),
    /// A single-quoted string literal with `''` already unescaped to `'`.
    Str(String),
    /// A bare identifier in value position that is *not* immediately
    /// followed by `(` — a reference to a context root (`github`, `env`, …).
    /// Root name resolution against the [`crate::context::Context`] is
    /// case-insensitive; an unrecognized root is a hard evaluation error
    /// (`UnrecognizedNamedValue`), unlike an unrecognized *property*, which
    /// evaluates to `null`. See "Contexts" in the Expressions reference.
    NamedValue(String),
    /// A function call `name(args…)`. Function names resolve
    /// case-insensitively (e.g. `fromJson`/`fromJSON`/`FROMJSON`).
    Call {
        /// The called function's name, as written (case is preserved; use
        /// case-insensitive comparison to identify which built-in this is).
        name: String,
        /// The argument expressions, in source order.
        args: Vec<Expr>,
    },
    /// `target[index]` or its dot-sugar `target.property` (property names are
    /// desugared to `Index(target, Str(name))`). See "Property dereference"
    /// and "Object filters" in the Expressions reference and
    /// `value::index_into` for the exact never-throws semantics.
    Index {
        /// The expression being indexed/dereferenced.
        target: Box<Expr>,
        /// The index expression (a `Str` literal for dot-sugar property
        /// access, or an arbitrary expression for `[...]` bracket access).
        index: Box<Expr>,
    },
    /// The object-filter wildcard `target.*` or `target[*]`: filters
    /// `target`'s elements/values into a new array. See "Object filters" in
    /// the Expressions reference.
    Wildcard {
        /// The expression being filtered.
        target: Box<Expr>,
    },
    /// Logical NOT (`!`). Always produces a genuine `Boolean` value, unlike
    /// `&&`/`||` which preserve operand values. Right-associative.
    Not(Box<Expr>),
    /// A binary operator application. See [`BinOp`] for the exact operator
    /// set and `eval::evaluate` for short-circuit semantics of `&&`/`||`.
    Binary {
        /// Which binary operator this is.
        op: BinOp,
        /// The left-hand operand.
        lhs: Box<Expr>,
        /// The right-hand operand.
        rhs: Box<Expr>,
    },
}

/// The binary operators, grouped by the four precedence tiers GitHub's
/// expression SDK defines (`Expressions2/Tokens/Token.cs`): relational binds
/// tighter than equality, which binds tighter than `&&`, which binds tighter
/// than `||`. All are left-to-right associative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `==`
    Eq,
    /// `!=` — defined as exactly `!(a == b)`.
    NotEq,
    /// `<`
    Lt,
    /// `<=` — defined as exactly `(a == b) || (a < b)`.
    Le,
    /// `>`
    Gt,
    /// `>=` — defined as exactly `(a == b) || (a > b)`.
    Ge,
    /// `&&` — short-circuiting, returns the first falsy operand's value or
    /// the last operand's value (not coerced to boolean).
    And,
    /// `||` — short-circuiting, returns the first truthy operand's value or
    /// the last operand's value (not coerced to boolean).
    Or,
}

/// Walks an expression tree looking for a call to any of the four status
/// check functions (`success`, `failure`, `cancelled`, `always`), anywhere in
/// the tree, including inside nested calls/operands.
///
/// Per the docs ("Status check functions" in the Expressions reference): "A
/// default status check of `success()` is applied unless you include one of
/// these functions." — the appearance of a status function *anywhere* in an
/// `if:` expression suppresses the implicit `success() &&` wrap, even nested
/// (`always() && x`, `!cancelled() && x`). Deciding *whether* to apply that
/// wrap for a given workflow key is `greenlit-engine`'s job (only `if:` keys
/// get it); this function only answers the tree-search question.
pub fn expr_calls_status_function(expr: &Expr) -> bool {
    const STATUS_FUNCTIONS: [&str; 4] = ["success", "failure", "cancelled", "always"];
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node {
            Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) | Expr::NamedValue(_) => {}
            Expr::Call { name, args } => {
                if STATUS_FUNCTIONS
                    .iter()
                    .any(|function| function.eq_ignore_ascii_case(name))
                {
                    return true;
                }
                stack.extend(args);
            }
            Expr::Index { target, index } => {
                stack.push(target);
                stack.push(index);
            }
            Expr::Wildcard { target } | Expr::Not(target) => stack.push(target),
            Expr::Binary { lhs, rhs, .. } => {
                stack.push(lhs);
                stack.push(rhs);
            }
        }
    }
    false
}
