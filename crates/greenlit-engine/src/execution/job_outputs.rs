//! Finalizing a job's declared `outputs:` after its steps run: evaluating each
//! output template against the final step context, redacting secrets, and
//! enforcing GitHub's size cap.
//!
//! GitHub evaluates `jobs.<id>.outputs` once the job's steps finish, against
//! the completed `steps` context, then exposes the values through
//! `needs.<id>.outputs` for direct dependents.
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idoutputs>

use indexmap::IndexMap;
use thiserror::Error;

use greenlit_expr::value::to_display_string;
use greenlit_expr::{Context, EvalError, RunStatus, evaluate};

use crate::execution::log_commands::Masker;
use crate::execution::outcome::{Conclusion, job_result_from_status};
use crate::outputs::{JobOutputsPlan, PlannedValue};

/// The size ceiling for the combined names and values of a job's outputs.
///
/// GitHub documents a 1 MiB cap for a step's job summary and applies
/// comparable ~1 MB limits to the output/env command files; a value larger
/// than this is rejected rather than silently truncated. Treated here as one
/// aggregate budget across the job's outputs.
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands#adding-a-job-summary>
pub const MAX_JOB_OUTPUTS_BYTES: usize = 1024 * 1024;

/// A failure finalizing a job's outputs.
#[derive(Debug, Error)]
pub enum JobOutputError {
    /// A deferred output expression failed to evaluate against the final
    /// context.
    #[error("could not evaluate job output '{name}': {source}")]
    Eval {
        /// The output whose expression failed.
        name: String,
        /// The underlying evaluation error.
        source: EvalError,
    },
    /// The combined outputs exceeded [`MAX_JOB_OUTPUTS_BYTES`].
    #[error("job outputs exceed the {limit}-byte limit")]
    TooLarge {
        /// The byte limit that was crossed.
        limit: usize,
    },
}

/// Evaluates every declared output against the final step `ctx`, redacting any
/// registered secret from each value, and enforcing the aggregate size cap.
///
/// Secret redaction is applied to the *stored* value, not only to logs: a
/// registered secret passed through a job output becomes `***` downstream,
/// matching GitHub's observed behavior that secrets cannot be smuggled between
/// jobs via outputs.
/// <https://docs.github.com/en/actions/security-for-github-actions/security-guides/using-secrets-in-github-actions#masking-secrets>
pub fn finalize_outputs(
    plan: &JobOutputsPlan,
    ctx: &Context,
    masker: &Masker,
) -> Result<IndexMap<String, String>, JobOutputError> {
    let mut outputs = IndexMap::with_capacity(plan.entries.len());
    let mut total = 0usize;
    for (name, planned) in &plan.entries {
        let raw = match &planned.value {
            PlannedValue::Static(value) => value.clone(),
            PlannedValue::Deferred(deferred) => {
                let value =
                    evaluate(&deferred.residual, ctx).map_err(|source| JobOutputError::Eval {
                        name: name.clone(),
                        source,
                    })?;
                to_display_string(&value)
            }
        };
        let value = masker.apply(&raw);
        total = total.saturating_add(name.len()).saturating_add(value.len());
        if total > MAX_JOB_OUTPUTS_BYTES {
            return Err(JobOutputError::TooLarge {
                limit: MAX_JOB_OUTPUTS_BYTES,
            });
        }
        outputs.insert(name.clone(), value);
    }
    Ok(outputs)
}

/// The job's terminal [`Conclusion`] for `needs.<id>.result`. A job whose own
/// `if:` suppressed it (or all of whose needed jobs were skipped) never ran
/// and is `skipped`; otherwise its rolling status maps directly.
pub fn job_result(ran: bool, final_status: RunStatus) -> Conclusion {
    if ran {
        job_result_from_status(final_status)
    } else {
        Conclusion::Skipped
    }
}
