//! Assembling the live `steps` and `needs` expression contexts from executed
//! steps and completed direct-dependency jobs, and merging matrix-leg outputs.
//!
//! These are the runtime counterparts to the plan-time context *slots* in
//! `crate::partial_eval`: once steps run and jobs complete, their real
//! outcomes and outputs populate the contexts that later `if:`/output
//! expressions evaluate against.
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#steps-context>
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#needs-context>

use indexmap::IndexMap;

use greenlit_expr::Value;

use crate::execution::outcome::Conclusion;
use crate::graph::JobId;

/// One completed step's contribution to the `steps` context. Only steps that
/// declared an `id:` appear in the context, matching GitHub.
#[derive(Debug, Clone)]
pub struct StepRecord {
    /// The step's `id:`.
    pub id: String,
    /// `steps.<id>.outcome`.
    pub outcome: Conclusion,
    /// `steps.<id>.conclusion`.
    pub conclusion: Conclusion,
    /// `steps.<id>.outputs` (from the step's `GITHUB_OUTPUT` writes).
    pub outputs: IndexMap<String, String>,
}

/// Builds the `steps` context object from the steps completed so far, in
/// declaration order.
pub fn build_steps_context(records: &[StepRecord]) -> Value {
    Value::object(
        records
            .iter()
            .map(|record| {
                let entry = Value::object(vec![
                    ("outputs".to_string(), string_object(&record.outputs)),
                    (
                        "outcome".to_string(),
                        Value::String(record.outcome.as_github_str().to_string()),
                    ),
                    (
                        "conclusion".to_string(),
                        Value::String(record.conclusion.as_github_str().to_string()),
                    ),
                ]);
                (record.id.clone(), entry)
            })
            .collect(),
    )
}

/// One completed direct-dependency job's contribution to the `needs` context.
#[derive(Debug, Clone)]
pub struct NeedRecord {
    /// The dependency job id.
    pub job: JobId,
    /// `needs.<id>.result`.
    pub result: Conclusion,
    /// `needs.<id>.outputs` (already matrix-merged for a matrix dependency).
    pub outputs: IndexMap<String, String>,
}

/// Builds the `needs` context object from the current job's *direct*
/// dependencies only. GitHub exposes only direct `needs`, never transitive
/// ones, and does so before downstream job conditions run.
pub fn build_needs_context(records: &[NeedRecord]) -> Value {
    Value::object(
        records
            .iter()
            .map(|record| {
                let entry = Value::object(vec![
                    (
                        "result".to_string(),
                        Value::String(record.result.as_github_str().to_string()),
                    ),
                    ("outputs".to_string(), string_object(&record.outputs)),
                ]);
                (record.job.0.clone(), entry)
            })
            .collect(),
    )
}

/// Merges the per-leg outputs of a matrix job into the single output map that
/// downstream `needs.<id>.outputs` sees. GitHub does not aggregate matrix
/// outputs; the value from the last leg to write a given key wins, so a
/// later-completing leg overwrites earlier legs for shared keys.
///
/// v0 runs jobs (and thus matrix legs) sequentially in DAG order, so "last"
/// is the last leg in iteration order. This is documented/observed behavior:
/// matrix jobs share one downstream `needs` entry and clobber each other's
/// outputs.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#needs-context>
pub fn merge_matrix_outputs<'a, I>(legs: I) -> IndexMap<String, String>
where
    I: IntoIterator<Item = &'a IndexMap<String, String>>,
{
    let mut merged: IndexMap<String, String> = IndexMap::new();
    for leg in legs {
        for (key, value) in leg {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

/// Converts a flat string map into an expression object with string values.
fn string_object(map: &IndexMap<String, String>) -> Value {
    Value::object(
        map.iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}
