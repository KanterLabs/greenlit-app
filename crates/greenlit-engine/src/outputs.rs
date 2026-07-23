//! [`JobOutputsPlan`]/[`PlannedValue`]: `jobs.<id>.outputs` modeled without
//! inventing values.
//!
//! `PHASE-1-engine-core.md` requires outputs to remain runtime-deferred
//! without invented values. Output *names* are fully static (declared in
//! the workflow file); output *values* are templates whose interpolations
//! this module partially evaluates with exactly the same machinery as `if:`
//! conditions (`crate::partial_eval`). GitHub's `needs` context exposes
//! those values only after the producing job completes:
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#needs-context>.

use greenlit_workflow::Span;
use indexmap::IndexMap;
use serde::Serialize;

use crate::condition::DeferredExpr;
use crate::json_shape::EvaluatedJson;
use crate::partial_eval::{FoldCtx, LocatedEvalError, TemplateFold, fold_template};
use crate::plan::{PlanSizeBudget, RetainedFieldError};

/// One job's `outputs:` block, plan-time resolved as far as possible.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct JobOutputsPlan {
    /// Declaration order preserved — the key *set* is the plan-time
    /// contract; a caller wanting a specific output's plan-time value looks
    /// it up by name here.
    pub entries: IndexMap<String, PlannedOutput>,
}

/// One declared output: its verbatim authored template text plus the
/// plan-time evaluation result.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedOutput {
    /// Location of the authored output value.
    pub span: Span,
    /// Verbatim authored text (including the `${{ }}` wrapper, unlike a
    /// condition's `source` — an output value keeps whatever literal text
    /// surrounds its placeholders, since it is a template, not a bare
    /// expression).
    pub source: String,
    /// The partial-evaluation result.
    pub value: PlannedValue,
}

/// One declared output's plan-time value.
#[derive(Debug, Clone, PartialEq)]
pub enum PlannedValue {
    /// A literal, or a template whose every interpolation folded
    /// statically — job outputs are always strings (docs: "job outputs are
    /// strings"), so this is always the final `ToString`-concatenated text,
    /// never a preserved non-string type.
    Static(String),
    /// Not fully resolved; same shape as a condition's residual — carries
    /// no invented value.
    Deferred(DeferredExpr),
}

impl Serialize for PlannedOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match &self.value {
            PlannedValue::Static(s) => EvaluatedJson {
                span: self.span.to_string(),
                source: &self.source,
                evaluation: "static",
                value: Some(s),
                residual: None,
                defers_on: None,
            }
            .serialize(serializer),
            PlannedValue::Deferred(d) => EvaluatedJson::<String> {
                span: self.span.to_string(),
                source: &self.source,
                evaluation: "deferred",
                value: None,
                residual: Some(&d.residual_text),
                defers_on: Some(&d.defers_on),
            }
            .serialize(serializer),
        }
    }
}

/// Folds every declared `outputs:` entry's raw template text into a
/// [`JobOutputsPlan`], preserving declaration order.
pub(crate) fn plan_outputs(
    raw_outputs: &[(String, String, Span)],
    ctx: &FoldCtx<'_>,
    size_budget: &mut PlanSizeBudget,
) -> Result<JobOutputsPlan, RetainedFieldError> {
    let mut entries = IndexMap::with_capacity(raw_outputs.len());
    for (name, raw, span) in raw_outputs {
        let value = match fold_template(raw, ctx).map_err(|source| LocatedEvalError {
            span: span.clone(),
            source,
        })? {
            TemplateFold::Static(v) => {
                PlannedValue::Static(greenlit_expr::value::to_display_string(&v))
            }
            TemplateFold::Deferred {
                residual,
                residual_text,
                defers_on,
            } => PlannedValue::Deferred(DeferredExpr {
                residual,
                residual_text,
                defers_on: defers_on.into_iter().collect(),
            }),
        };
        let planned = PlannedOutput {
            span: span.clone(),
            source: raw.clone(),
            value,
        };
        size_budget.add(&(name.as_str(), &planned), span)?;
        entries.insert(name.clone(), planned);
    }
    Ok(JobOutputsPlan { entries })
}
