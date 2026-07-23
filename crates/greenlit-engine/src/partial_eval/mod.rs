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
mod env;
mod fold;
mod printer;
mod template;

pub(crate) use env::{EnvChain, build_env_chain, extend_env_chain};
pub(crate) use fold::fold_expr;
pub(crate) use printer::{pretty_print, value_to_literal_expr};
pub(crate) use template::fold_template;

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::Path;
use std::sync::Arc;

use greenlit_expr::{Context, EntryKind, Expr, HashFilesFs, OpenedDir, Value};
use greenlit_workflow::model::value::ScalarOrExpr;

use crate::convert::scalar_to_value;
use crate::defer::DeferReason;
use crate::graph::JobId;

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

/// Context roots and runtime-slot metadata available to one partial
/// evaluation. `needs` and `steps` use an empty static object for missing
/// properties plus explicit slot metadata so only values that can actually
/// materialize later are deferred.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StaticRoots<'a> {
    /// The `github` context root.
    pub github: &'a Value,
    /// Event-specific `github.*` properties whose values remain unknown at
    /// planning time even though their documented types are fixed.
    pub github_deferred: &'a BTreeSet<String>,
    /// The `vars` context root.
    pub vars: &'a Value,
    /// Static portion of the `needs` context. Runtime result/output values
    /// are represented by [`NeedsContextSlots`] instead of invented data.
    pub needs: &'a Value,
    /// Direct dependency slots that can materialize at runtime.
    pub needs_slots: &'a NeedsContextSlots,
    /// The `matrix` context root (`Null` outside a matrix leg).
    pub matrix: &'a Value,
    /// Whether `matrix` is pending expansion rather than statically null.
    pub matrix_deferred: bool,
    /// The `strategy` context root (`Null` outside a matrix leg).
    pub strategy: &'a Value,
    /// Which fixed `strategy` properties still require runtime data.
    pub strategy_deferred: StrategyDeferred,
    /// Static portion of the `steps` context.
    pub steps: &'a Value,
    /// Prior authored step IDs whose status/outputs can exist here.
    pub steps_slots: &'a StepsContextSlots,
    /// The `inputs` context root.
    pub inputs: &'a Value,
}

/// Direct `needs` entries visible to one job, with the output names each
/// dependency actually declares. A missing job or undeclared output is a
/// statically absent property, while `result` and declared outputs remain
/// runtime slots.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeclaredOutputs {
    names: std::sync::Arc<[String]>,
    canonical_names: std::sync::Arc<std::collections::HashSet<String>>,
}

impl DeclaredOutputs {
    pub(crate) fn new(names: Vec<String>) -> Self {
        let canonical_names = names
            .iter()
            .map(|name| greenlit_expr::value::ordinal_ignore_case_key(name))
            .collect();
        Self {
            names: names.into(),
            canonical_names: std::sync::Arc::new(canonical_names),
        }
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.canonical_names
            .contains(&greenlit_expr::value::ordinal_ignore_case_key(name))
    }

    fn names(&self) -> &[String] {
        &self.names
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct NeedsContextSlots {
    entries: Vec<(JobId, DeclaredOutputs)>,
    by_job: HashMap<String, usize>,
}

impl NeedsContextSlots {
    pub(crate) fn new(entries: Vec<(JobId, DeclaredOutputs)>) -> Self {
        let by_job = entries
            .iter()
            .enumerate()
            .map(|(index, (job, _))| (greenlit_expr::value::ordinal_ignore_case_key(&job.0), index))
            .collect();
        Self { entries, by_job }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Builds the known shape of GitHub's direct-dependency map without
    /// inventing runtime results or output values. Declared slots contain
    /// `Null` placeholders only as structural sentinels; the classifier
    /// prevents those runtime paths from reaching static evaluation.
    /// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#needs-context>
    pub(crate) fn static_value(&self) -> Value {
        Value::object(
            self.entries
                .iter()
                .map(|(job, outputs)| {
                    let output_shape = Value::object(
                        outputs
                            .names()
                            .iter()
                            .map(|name| (name.clone(), Value::Null))
                            .collect(),
                    );
                    (
                        job.0.clone(),
                        Value::object(vec![
                            ("outputs".to_string(), output_shape),
                            ("result".to_string(), Value::Null),
                        ]),
                    )
                })
                .collect(),
        )
    }

    pub(crate) fn canonical_job_id(&self, name: &str) -> Option<&JobId> {
        self.by_job
            .get(&greenlit_expr::value::ordinal_ignore_case_key(name))
            .and_then(|index| self.entries.get(*index))
            .map(|(job, _)| job)
    }

    pub(crate) fn has_declared_outputs(&self, name: &str) -> bool {
        self.outputs_for(name)
            .is_some_and(|outputs| !outputs.names().is_empty())
    }

    pub(crate) fn declares_output(&self, job: &str, output: &str) -> bool {
        self.outputs_for(job)
            .is_some_and(|outputs| outputs.contains(output))
    }

    fn outputs_for(&self, name: &str) -> Option<&DeclaredOutputs> {
        self.by_job
            .get(&greenlit_expr::value::ordinal_ignore_case_key(name))
            .and_then(|index| self.entries.get(*index))
            .map(|(_, outputs)| outputs)
    }
}

/// Step IDs whose `steps.<id>` context entry exists at a particular
/// evaluation point. Step fields see only earlier IDs; job outputs see all
/// authored IDs after the step sequence completes.
#[derive(Debug, Clone, Default)]
pub(crate) struct StepsContextSlots {
    ids: Vec<String>,
}

impl StepsContextSlots {
    pub(crate) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.ids
            .iter()
            .any(|id| greenlit_expr::value::ordinal_ignore_case_eq(id, name))
    }

    pub(crate) fn add(&mut self, id: String) {
        self.ids.push(id);
    }
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
        Context::new(Arc::new(StaticFs))
            .with_github(self.roots.github.clone())
            .with_env(self.env.resolved.clone())
            .with_vars(self.roots.vars.clone())
            .with_needs(self.roots.needs.clone())
            .with_matrix(self.roots.matrix.clone())
            .with_strategy(self.roots.strategy.clone())
            .with_steps(self.roots.steps.clone())
            .with_inputs(self.roots.inputs.clone())
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
    fn open_dir(&self, _path: &Path) -> io::Result<OpenedDir<'_>> {
        Err(unavailable_io_error())
    }
    fn entry_kind(&self, _path: &Path) -> io::Result<EntryKind> {
        Err(unavailable_io_error())
    }
    fn hash_file_sha256(
        &self,
        _path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
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
