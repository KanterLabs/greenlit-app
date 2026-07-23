//! Ordered `env:` layering and step-time mutation tracking.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use greenlit_expr::Value;
use greenlit_expr::value::to_display_string;
use greenlit_workflow::Spanned;
use greenlit_workflow::model::value::ScalarOrExpr;

use crate::defer::DeferReason;

use super::{FoldCtx, Folded, LocatedEvalError, StaticRoots, fold_scalar_or_expr};

/// The resolved `env:` chain for one job or step: workflow-then-job(-then-
/// step) layers already folded, split into names whose value is fully
/// known (`resolved`) and names whose own definition is itself deferred
/// (`deferred`, with the reasons why). A name absent from both was never
/// declared anywhere in the chain: it is statically absent until an
/// executable predecessor makes `GITHUB_ENV` mutation possible, after which
/// it defers with [`DeferReason::DynamicEnv`].
#[derive(Debug, Clone)]
pub(crate) struct EnvChain {
    pub(super) resolved: Value,
    deferred: Rc<HashMap<String, BTreeSet<DeferReason>>>,
    runtime_mutable: bool,
    stable_overrides: Rc<HashSet<String>>,
}

impl EnvChain {
    /// An env chain with nothing declared anywhere. An absent `env.*`
    /// reference folds to GitHub's empty missing-property value until this
    /// chain is marked runtime-mutable. Used by `crate::matrix`/
    /// `crate::runner` folding contexts that have no `env:` to layer (matrix
    /// axis values, `runs-on` labels), and by tests.
    pub(crate) fn empty() -> Self {
        EnvChain {
            resolved: Value::case_sensitive_object(vec![]),
            deferred: Rc::new(HashMap::new()),
            runtime_mutable: false,
            stable_overrides: Rc::new(HashSet::new()),
        }
    }

    pub(crate) fn deferred_reasons(&self, name: &str) -> Option<&BTreeSet<DeferReason>> {
        self.deferred.get(name)
    }

    /// Returns whether a prior step may have changed this name through
    /// `GITHUB_ENV`. A current step-level `env:` entry is a stable override
    /// for that step, even when an outer value with the same name is mutable.
    pub(crate) fn is_runtime_mutable(&self, name: &str) -> bool {
        self.runtime_mutable && !self.stable_overrides.contains(name)
    }

    /// Marks outer workflow/job environment values as potentially changed
    /// by a previously executing step. GitHub applies values written to
    /// `GITHUB_ENV` to every subsequent step, but not to the writing step.
    /// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands#setting-an-environment-variable>
    pub(crate) fn after_executable_step(mut self) -> Self {
        self.runtime_mutable = true;
        self.stable_overrides = Rc::new(HashSet::new());
        self
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
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#env>
pub(crate) fn build_env_chain(
    layers: &[&[(Spanned<String>, Spanned<ScalarOrExpr>)]],
    roots: StaticRoots<'_>,
) -> Result<EnvChain, LocatedEvalError> {
    let mut chain = EnvChain::empty();
    for layer in layers {
        chain = extend_env_chain(&chain, layer, roots, false)?;
    }
    Ok(chain)
}

/// Applies one additional `env:` layer to a previously assembled chain.
/// The complete layer is evaluated against the unmodified outer chain so
/// sibling entries cannot reference one another. When `stable_overrides` is
/// true, names declared by the layer are treated as current-step overrides
/// and cannot be shadowed by a prior `GITHUB_ENV` write.
pub(crate) fn extend_env_chain(
    outer: &EnvChain,
    layer: &[(Spanned<String>, Spanned<ScalarOrExpr>)],
    roots: StaticRoots<'_>,
    stable_overrides: bool,
) -> Result<EnvChain, LocatedEvalError> {
    // An absent layer changes neither lookup semantics nor mutability. Keep
    // the existing `Value`/map allocations shared instead of rebuilding a
    // potentially large outer environment for every job field and step.
    if layer.is_empty() {
        return Ok(outer.clone());
    }

    let layer_ctx = FoldCtx {
        roots,
        env: outer,
        secrets_forbidden: false,
    };
    let mut folded_layer = Vec::with_capacity(layer.len());
    for (name, value) in layer {
        folded_layer.push((
            name.value.clone(),
            fold_scalar_or_expr(&value.value, &layer_ctx).map_err(|source| LocatedEvalError {
                span: value.span.clone(),
                source,
            })?,
        ));
    }

    let mut resolved_entries = match &outer.resolved {
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| Some((key.to_string(), value.clone())))
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    // Layering removes an overridden key from its old insertion position and
    // appends the replacement. A position index plus tombstones preserves
    // that order in O(outer + layer) time; repeated `Vec::retain` made a
    // repository-authored env map quadratic before any step was planned.
    let mut resolved_positions = HashMap::with_capacity(resolved_entries.len() + layer.len());
    for (index, entry) in resolved_entries.iter().enumerate() {
        if let Some((name, _)) = entry {
            resolved_positions.insert(name.clone(), index);
        }
    }
    let mut deferred = outer.deferred.as_ref().clone();
    let mut stable_names = outer.stable_overrides.as_ref().clone();
    for (name, folded) in folded_layer {
        if let Some(index) = resolved_positions.remove(&name)
            && let Some(slot) = resolved_entries.get_mut(index)
        {
            *slot = None;
        }
        deferred.remove(&name);
        stable_names.remove(&name);
        match folded {
            Folded::Value(value) => {
                resolved_positions.insert(name.clone(), resolved_entries.len());
                resolved_entries.push(Some((
                    name.clone(),
                    Value::String(to_display_string(&value)),
                )));
            }
            Folded::Residual { defers_on, .. } => {
                deferred.insert(name.clone(), defers_on);
            }
        }
        if stable_overrides {
            stable_names.insert(name);
        }
    }

    Ok(EnvChain {
        resolved: Value::case_sensitive_object(resolved_entries.into_iter().flatten().collect()),
        deferred: Rc::new(deferred),
        runtime_mutable: outer.runtime_mutable,
        stable_overrides: Rc::new(stable_names),
    })
}
