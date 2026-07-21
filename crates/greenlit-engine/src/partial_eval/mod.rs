//! The partial evaluator: folds an `if:`/output/`env:` expression against
//! the contexts already known at plan time, producing either a concrete
//! [`greenlit_expr::Value`] or a residual [`greenlit_expr::Expr`] plus the
//! [`DeferReason`]s it could not get past.
//!
//! This implements `PHASE-1-engine-core.md`'s rule to evaluate statically
//! known expressions and preserve runtime-deferred residuals. It is split
//! into focused single-purpose submodules:
//!
//! - [`fold`] — the recursive folder ([`fold_expr`]). One evaluator, reused:
//!   whenever a subtree turns out to reference *nothing* deferred, it hands
//!   the whole subtree to [`greenlit_expr::evaluate`] rather than
//!   reimplementing index/wildcard/coercion semantics a second time; the
//!   slow, structural path only kicks in for node shapes that must preserve
//!   short-circuit semantics across a partially-deferred tree.
//! - [`calls`] — preserves built-ins' lazy argument semantics while folding
//!   calls whose other arguments require runtime data.
//! - [`classify`] — finds runtime dependencies without evaluating the tree.
//! - [`template`] — folds a raw template string (`env:` entries, job output
//!   values: zero or more `${{ }}` placeholders possibly mixed with
//!   literal text), reusing [`fold_expr`] per placeholder.
//! - [`printer`] — regenerates expression source text from a folded/
//!   residual tree for display; targets trees this module itself produces,
//!   not a fully general round-trip printer for arbitrary hand-built trees.

mod calls;
mod classify;
mod fold;
mod printer;
mod template;

pub(crate) use fold::fold_expr;
pub(crate) use printer::{pretty_print, value_to_literal_expr};
pub(crate) use template::fold_template;

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::Path;
use std::rc::Rc;

use greenlit_expr::value::to_display_string;
use greenlit_expr::{Context, DirEntry, Expr, HashFilesFs, Value};
use greenlit_workflow::Spanned;
use greenlit_workflow::model::value::ScalarOrExpr;

use crate::convert::scalar_to_value;
use crate::defer::DeferReason;

/// The result of folding one [`Expr`] against a [`FoldCtx`]: either it
/// collapsed all the way to a value, or some part of it could not be
/// decided yet.
#[derive(Debug, Clone)]
pub(crate) enum Folded {
    /// Fully resolved.
    Value(Value),
    /// Not fully resolved: the (possibly partially-folded) remaining tree,
    /// plus every reason it could not go further.
    Residual {
        expr: Expr,
        defers_on: BTreeSet<DeferReason>,
    },
}

/// The result of folding a template string (zero or more `${{ }}`
/// placeholders mixed with literal text) — used for `env:` entries and job
/// output values, which (unlike `if:`) are never a single bare expression.
#[derive(Debug, Clone)]
pub(crate) enum TemplateFold {
    /// Fully resolved. Preserves the single placeholder's native type when
    /// the template is exactly one whole `${{ }}` with no surrounding
    /// literal text; a template with any literal text or more than one
    /// placeholder always resolves to a [`Value::String`] (`ToString`
    /// concatenation, matching the template-interpolation rule).
    Static(Value),
    /// Not fully resolved.
    Deferred {
        residual: Expr,
        residual_text: String,
        defers_on: BTreeSet<DeferReason>,
    },
}

/// Everything a partial evaluation could go wrong on.
#[derive(Debug, thiserror::Error)]
pub enum PartialEvalError {
    /// A `secrets.*` reference reached partial evaluation for a job-level
    /// `if:`. The job-condition availability validator normally rejects it
    /// first, together with every other context GitHub does not permit at
    /// that workflow key; this remains a defensive backstop.
    #[error(
        "'secrets' is not allowed in a job-level `if:` condition (not in GitHub's job-`if` context-availability list); move the check into a step-level `if:` instead"
    )]
    SecretsForbiddenInJobCondition,
    /// An embedded `${{ ... }}` expression failed to parse.
    #[error("could not parse expression: {0}")]
    ExprParse(#[from] greenlit_expr::ParseError),
    /// A fully-static subtree failed to evaluate (e.g. a malformed
    /// `format()` pattern, or `fromJSON()` given invalid JSON) — a genuine
    /// plan-time error, since every input the subtree touches is already
    /// known.
    #[error("could not evaluate a fully-static expression: {0}")]
    Eval(#[from] greenlit_expr::EvalError),
    /// A context-sensitive field folded successfully but produced a value
    /// outside that field's documented type/domain.
    #[error("static expression must evaluate to {expected}")]
    InvalidStaticValue {
        /// The expected value domain.
        expected: &'static str,
    },
}

/// A partial-evaluation failure paired with the exact authored field.
#[derive(Debug)]
pub(crate) struct LocatedEvalError {
    pub(crate) span: greenlit_workflow::Span,
    pub(crate) source: PartialEvalError,
}

/// The five contexts this crate can materialize a concrete [`Value`] for at
/// plan time: `github`, `vars`, `matrix`, `strategy` (both `Null` outside a
/// matrix leg), and `inputs`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StaticRoots<'a> {
    /// The `github` context root.
    pub github: &'a Value,
    /// The `vars` context root.
    pub vars: &'a Value,
    /// A completed direct-dependency context. `None` means the job's
    /// prerequisites have not finished and every `needs.*` lookup defers.
    pub needs: Option<&'a Value>,
    /// The `matrix` context root (`Null` outside a matrix leg).
    pub matrix: &'a Value,
    /// Whether `matrix` is pending expansion rather than statically null.
    pub matrix_deferred: bool,
    /// The `strategy` context root (`Null` outside a matrix leg).
    pub strategy: &'a Value,
    /// Which fixed `strategy` properties still require runtime data.
    pub strategy_deferred: StrategyDeferred,
    /// The `inputs` context root.
    pub inputs: &'a Value,
}

/// Per-property availability for GitHub's fixed-shape `strategy` context.
/// Keeping these flags separate avoids turning a known `job-total` into a
/// residual merely because `fail-fast` depends on a prerequisite output.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StrategyDeferred {
    pub fail_fast: bool,
    pub job_index: bool,
    pub job_total: bool,
    pub max_parallel: bool,
}

impl StrategyDeferred {
    pub(crate) fn any(self) -> bool {
        self.fail_fast || self.job_index || self.job_total || self.max_parallel
    }

    pub(crate) fn field(self, name: &str) -> bool {
        if name.eq_ignore_ascii_case("fail-fast") {
            self.fail_fast
        } else if name.eq_ignore_ascii_case("job-index") {
            self.job_index
        } else if name.eq_ignore_ascii_case("job-total") {
            self.job_total
        } else if name.eq_ignore_ascii_case("max-parallel") {
            self.max_parallel
        } else {
            false
        }
    }
}

/// The resolved `env:` chain for one job or step: workflow-then-job(-then-
/// step) layers already folded, split into names whose value is fully
/// known (`resolved`) and names whose own definition is itself deferred
/// (`deferred`, with the reasons why). A name absent from both was never
/// declared anywhere in the chain and defers with
/// [`DeferReason::DynamicEnv`] because runtime commands may still set it.
#[derive(Debug, Clone)]
pub(crate) struct EnvChain {
    resolved: Value,
    deferred: HashMap<String, BTreeSet<DeferReason>>,
}

impl EnvChain {
    /// An env chain with nothing declared anywhere — every `env.*`
    /// reference against it defers as [`DeferReason::DynamicEnv`]. Used by
    /// `crate::matrix`/`crate::runner` folding contexts that have no `env:`
    /// to layer (matrix axis values, `runs-on` labels), and by tests.
    pub(crate) fn empty() -> Self {
        EnvChain {
            resolved: Value::object(vec![]),
            deferred: HashMap::new(),
        }
    }

    pub(crate) fn is_declared(&self, name: &str) -> bool {
        self.deferred.contains_key(name)
            || matches!(&self.resolved, Value::Object(o) if o.get(name).is_some())
    }

    pub(crate) fn deferred_reasons(&self, name: &str) -> Option<&BTreeSet<DeferReason>> {
        self.deferred.get(name)
    }
}

/// Folds each `env:` layer (workflow, then job, then step — later layers
/// override earlier ones by key, matching GitHub's env-layering rule) into
/// one [`EnvChain`]. Layers should be passed outermost-first (workflow,
/// job, step). Job-instance fields use `[workflow_env, job_env]`; step
/// fields use all three. Job-level `if:` does not use this chain because
/// GitHub does not make the `env` context available at that workflow key.
///
/// Each layer is evaluated against the completed outer layers, never
/// against sibling entries in the same map. This preserves GitHub's
/// workflow < job < step precedence while honoring its rule that variables
/// in one `env` map cannot be defined in terms of other variables in that
/// same map.
/// https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#env
pub(crate) fn build_env_chain(
    layers: &[&[(Spanned<String>, Spanned<ScalarOrExpr>)]],
    roots: StaticRoots<'_>,
) -> Result<EnvChain, LocatedEvalError> {
    let mut resolved_entries: Vec<(String, Value)> = Vec::new();
    let mut deferred: HashMap<String, BTreeSet<DeferReason>> = HashMap::new();
    for layer in layers {
        let outer_chain = EnvChain {
            resolved: Value::object(resolved_entries.clone()),
            deferred: deferred.clone(),
        };
        let layer_ctx = FoldCtx {
            roots,
            env: &outer_chain,
            secrets_forbidden: false,
        };
        let mut folded_layer = Vec::with_capacity(layer.len());
        for (name, value) in *layer {
            folded_layer.push((
                name.value.clone(),
                fold_scalar_or_expr(&value.value, &layer_ctx).map_err(|source| {
                    LocatedEvalError {
                        span: value.span.clone(),
                        source,
                    }
                })?,
            ));
        }
        for (name, folded) in folded_layer {
            resolved_entries.retain(|(key, _)| key != &name);
            deferred.remove(&name);
            match folded {
                Folded::Value(v) => {
                    resolved_entries.push((name, Value::String(to_display_string(&v))));
                }
                Folded::Residual { defers_on, .. } => {
                    deferred.insert(name, defers_on);
                }
            }
        }
    }
    Ok(EnvChain {
        resolved: Value::object(resolved_entries),
        deferred,
    })
}

/// Everything [`fold_expr`]/[`fold_template`] need: the static context
/// values, the resolved `env:` chain, and whether a bare `secrets`
/// reference is a hard error here (job-level `if:`) or merely deferred
/// (everywhere else).
#[derive(Debug, Clone, Copy)]
pub(crate) struct FoldCtx<'a> {
    /// The static context values.
    pub roots: StaticRoots<'a>,
    /// The resolved `env:` chain in effect here.
    pub env: &'a EnvChain,
    /// `true` only for a job-level `if:`, where a `secrets.*` reference is
    /// a hard error rather than a deferral.
    pub secrets_forbidden: bool,
}

impl FoldCtx<'_> {
    /// Builds the real evaluator's [`Context`] for the fast path: `github`,
    /// `env` (the resolved chain), `vars`, `matrix`, `strategy`, and `inputs` are the
    /// only roots any subtree reaching this path can have touched — see
    /// this module's doc comment.
    pub(crate) fn static_context(&self) -> Context {
        let context = Context::new(Rc::new(StaticFs))
            .with_github(self.roots.github.clone())
            .with_env(self.env.resolved.clone())
            .with_vars(self.roots.vars.clone())
            .with_matrix(self.roots.matrix.clone())
            .with_strategy(self.roots.strategy.clone())
            .with_inputs(self.roots.inputs.clone());
        match self.roots.needs {
            Some(needs) => context.with_needs(needs.clone()),
            None => context,
        }
    }
}

/// A [`HashFilesFs`] that always fails — plumbed into [`Context`] only
/// because its constructor requires one; `hashFiles()` calls are always
/// classified as deferred (never reach the fast path that would actually
/// invoke this), so this is a defensive placeholder, not a real filesystem
/// seam.
#[derive(Debug)]
struct StaticFs;

impl HashFilesFs for StaticFs {
    fn workspace_root(&self) -> &Path {
        Path::new("/")
    }
    fn read_dir(&self, _path: &Path) -> io::Result<Vec<DirEntry>> {
        Err(unavailable_io_error())
    }
    fn read_file(&self, _path: &Path) -> io::Result<Vec<u8>> {
        Err(unavailable_io_error())
    }
}

fn unavailable_io_error() -> io::Error {
    io::Error::other("hashFiles() is not available during plan-time partial evaluation")
}

/// Folds a `ScalarOrExpr` (an `env:`/`with:`-shaped field) into a [`Folded`].
pub(crate) fn fold_scalar_or_expr(
    v: &ScalarOrExpr,
    ctx: &FoldCtx<'_>,
) -> Result<Folded, PartialEvalError> {
    match v {
        ScalarOrExpr::Literal(s) => Ok(Folded::Value(scalar_to_value(s))),
        ScalarOrExpr::Expression(raw) => match fold_template(raw, ctx)? {
            TemplateFold::Static(v) => Ok(Folded::Value(v)),
            TemplateFold::Deferred {
                residual,
                defers_on,
                ..
            } => Ok(Folded::Residual {
                expr: residual,
                defers_on,
            }),
        },
    }
}
