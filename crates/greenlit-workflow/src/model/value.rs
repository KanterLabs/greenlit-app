//! Value-position types shared across the workflow model: literal-or-
//! expression scalars, the generic recursive YAML value used for matrix
//! axes and similar free-form structure, and the recognized-but-rejected
//! construct marker.

use crate::span::Span;
use crate::span::Spanned;
pub use crate::yaml::scalar::YamlScalar;

/// A scalar as it appears in a workflow file: either a literal value
/// (typed per GitHub's YAML scalar rules, see [`YamlScalar`]) or raw source
/// text containing at least one `${{ … }}` expression placeholder.
///
/// Evaluating the `Expression` case — including the template-layer rule
/// that a scalar consisting of *exactly one* `${{ … }}` placeholder
/// preserves the expression's own type instead of stringifying it (design
/// memo §1.5) — is `greenlit-expr`/`greenlit-engine`'s job, not this
/// crate's; `greenlit-workflow` only classifies and preserves.
///
/// Classification is intentionally a simple substring check for `${{`.
/// The parser then performs quote-aware wrapper segmentation, parses every
/// inner expression with `greenlit-expr`, and applies the workflow key's
/// context/function policy before returning the model.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarOrExpr {
    /// No `${{` appears anywhere in the raw text; resolved to a typed
    /// literal per GitHub's YAML scalar rules.
    Literal(YamlScalar),
    /// The raw source text, verbatim, containing one or more `${{ … }}`
    /// placeholders (and possibly literal text around them).
    Expression(String),
}

impl ScalarOrExpr {
    /// Classify raw scalar source text: any occurrence of `${{` makes the
    /// whole value an [`ScalarOrExpr::Expression`]; otherwise it is resolved
    /// as a literal via `literal`.
    pub(crate) fn classify(raw: &str, literal: impl FnOnce() -> YamlScalar) -> Self {
        if raw.contains("${{") {
            ScalarOrExpr::Expression(raw.to_owned())
        } else {
            ScalarOrExpr::Literal(literal())
        }
    }
}

/// A generic, recursive YAML value: used wherever the workflow schema
/// allows genuinely free-form structure (matrix axis values, `include`/
/// `exclude` entries, `workflow_dispatch` input defaults, and any
/// recognized-but-not-deeply-modeled trigger's configuration).
#[derive(Debug, Clone, PartialEq)]
pub enum YamlValue {
    /// A scalar leaf (literal or expression text).
    Scalar(ScalarOrExpr),
    /// A YAML sequence.
    Sequence(Vec<Spanned<YamlValue>>),
    /// A YAML mapping, insertion-ordered.
    Mapping(Vec<(Spanned<String>, Spanned<YamlValue>)>),
}

/// A GitHub Actions construct that Greenlit recognizes syntactically —
/// parsing succeeds and the construct's location is preserved — but does
/// not execute in v0 (`greenlit-v0-spec.md` "Out (v0)": `concurrency`,
/// environments/deployments, reusable workflows, OIDC).
///
/// `greenlit-workflow` only records that the construct was present and
/// where; producing the precise "`<name>`: not in v0" rejection message is
/// `greenlit-engine`'s planning-stage job (`PHASE-1-engine-core.md`
/// greenlit-workflow section: "parsing succeeds, planning fails with a
/// precise 'not in v0' message naming the construct and its location").
#[derive(Debug, Clone, PartialEq)]
pub struct UnsupportedConstruct {
    /// The construct's name, as it should appear in the eventual rejection
    /// message (e.g. `"concurrency"`, `"workflow_call"`, `"environment"`,
    /// `"reusable workflow call (jobs.<id>.uses)"`).
    pub name: &'static str,
    /// Where the construct appears.
    pub location: Span,
}
