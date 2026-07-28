//! Precomputes each job step's `GITHUB_ACTION` value.
//!
//! Verified against the pinned runner (`actions/runner` v2.336.0, pinned
//! release — see `crate::executor::actions::node_runtime` module docs):
//! `GITHUB_ACTION` is set per step from `actionStep.Action.Name`
//! (`StepsRunner.cs:115-119`), which is either the step's explicit `id:` or,
//! when absent, a runner-generated default id assigned to *every* step —
//! even ones a workflow author never gives an `id:` — by
//! `WorkflowTemplateConverter.ConvertToSteps`
//! (`WFP_WorkflowTemplateConverter.cs:1579-1638`) using `IdBuilder`
//! (`WFP_IdBuilder.cs:91-124`).

use std::collections::HashSet;

use greenlit_actions::ActionRef;
use greenlit_engine::{StepKind, StepPlan};

/// Precomputes every step's `GITHUB_ACTION` value for a whole job in one
/// pass: an explicit `id:` wins outright (GitHub also rejects an authored
/// id starting with the reserved `__` prefix, so it can never collide with
/// a generated candidate below); otherwise a `run:` step's raw candidate is
/// `run`, and an `owner/repo@ref`-form `uses:` step's is `owner/repo` (a
/// subdirectory, if any, is dropped) — sanitized to `[A-Za-z0-9_-]` (any
/// other byte, including the `/` a `uses:` id carries, becomes `_`) and
/// prefixed `__`. A repeated candidate within the same job gets `_2`, `_3`,
/// ... appended (first occurrence bare, matching `IdBuilder.Build`'s
/// `attempt == 1` case), counted across the whole job in step order.
///
/// This is purely a function of the authored plan, never runtime data, so
/// it runs once per job rather than once per phase — every phase of the
/// same step (pre/main/post, `crate::executor::step::run_pre_steps`/
/// `execute_step`) reuses the same cached value instead of re-deriving it.
///
/// A `uses:` step this crate cannot reduce to an `owner/repo` pair — a
/// local (`./...`) or `docker://...` reference — falls back to the same
/// `run` candidate the real runner's own `ConvertToSteps` falls back to
/// when none of *its* branches produce an id. That fallback is honest for
/// a `run:` step (the runner's genuine catch-all case), but the real
/// runner actually gives a local/docker `uses:` step its own specific id
/// (the image reference, or a `self` alias) rather than reaching that
/// fallback — this task's citations only verified the `run` and
/// `owner/repo` cases against the pinned runner, so local/docker `uses:`
/// id generation is a deliberate, documented gap rather than a guessed
/// value.
///
/// Composite nuance (also citation-verified): a step nested inside a
/// composite action keeps the *outer* step's `GITHUB_ACTION` rather than
/// generating its own — `CompositeActionHandler` never re-sets
/// `github.action` for a nested step. This function only ever sees a job's
/// top-level `steps:`, so it has nothing to compute for nested steps in the
/// first place; propagating the outer value down into
/// `crate::executor::actions::composite`'s nested execution is the
/// remaining half of that nuance and is out of scope for this change (see
/// the citation comment at the composite dispatch site in
/// `crate::executor::step`).
pub(crate) fn compute_step_action_ids(steps: &[StepPlan]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::new();
    steps
        .iter()
        .map(|step| match &step.id {
            Some(id) => id.clone(),
            None => dedup_generated_id(&raw_generated_id(step), &mut used),
        })
        .collect()
}

/// The pre-sanitization, pre-`__`-prefix candidate for a step with no
/// explicit `id:`.
fn raw_generated_id(step: &StepPlan) -> String {
    match &step.kind {
        StepKind::Run { .. } => "run".to_string(),
        StepKind::Uses { reference, .. } => match ActionRef::parse(reference) {
            Ok(ActionRef::Repository(r)) => format!("{}/{}", r.owner, r.repo),
            _ => "run".to_string(),
        },
    }
}

/// Sanitizes `raw`, prefixes it `__`, and resolves any collision against
/// `used` with a numeric suffix — `WFP_IdBuilder.cs`'s `AppendSegment`
/// (sanitization) and `Build` (suffix) combined into one step, since this
/// crate never needs the general multi-segment ID builder the runner also
/// uses for job IDs.
fn dedup_generated_id(raw: &str, used: &mut HashSet<String>) -> String {
    let base = format!("__{}", sanitize_id_segment(raw));
    if used.insert(base.clone()) {
        return base;
    }
    let mut attempt = 2;
    loop {
        let candidate = format!("{base}_{attempt}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        attempt += 1;
    }
}

/// Replaces every byte outside `[A-Za-z0-9_-]` with `_` — the runner's own
/// legal-character set for a generated (or explicit) step/job ID
/// (`WFP_IdBuilder.cs`'s `AppendSegment`).
fn sanitize_id_segment(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
