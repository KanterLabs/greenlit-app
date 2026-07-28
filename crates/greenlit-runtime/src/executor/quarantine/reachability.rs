use std::collections::{BTreeMap, BTreeSet};

use greenlit_engine::{
    ExecutionPlan, JobPlan, MatrixLeg, MatrixPlan, PlannedCond, StaticSkip, StepPlan,
};

/// Planner-proven reachability for the selected plan.
#[derive(Debug, Clone, Default)]
pub struct PlanReachability {
    jobs: BTreeMap<String, ReachableJob>,
}

#[derive(Debug, Clone, Default)]
struct ReachableJob {
    template: bool,
    legs: BTreeSet<usize>,
    steps: BTreeSet<usize>,
}

impl PlanReachability {
    /// Whether at least one selected instance of `job` can execute.
    #[must_use]
    pub fn job_reachable(&self, job: &str) -> bool {
        self.jobs.contains_key(job)
    }

    /// Whether at least one selected instance can execute the authored step
    /// at `index`.
    #[must_use]
    pub fn step_reachable(&self, job: &str, index: usize) -> bool {
        self.jobs
            .get(job)
            .is_some_and(|reachable| reachable.steps.contains(&index))
    }

    /// Whether any selected job instance can execute.
    #[must_use]
    pub fn any_job_reachable(&self) -> bool {
        !self.jobs.is_empty()
    }

    pub(crate) fn template_reachable(&self, job: &str) -> bool {
        self.jobs
            .get(job)
            .is_some_and(|reachable| reachable.template)
    }

    pub(crate) fn leg_reachable(&self, job: &str, index: usize) -> bool {
        self.jobs
            .get(job)
            .is_some_and(|reachable| reachable.legs.contains(&index))
    }
}

/// Returns the exact static reachability already proven by the planner.
///
/// Deferred conditions and malformed/ambiguous selected-matrix shapes remain
/// conservatively reachable; the capability assessment separately emits
/// their non-forceable ambiguity/security findings.
#[must_use]
pub fn plan_reachability(plan: &ExecutionPlan) -> PlanReachability {
    let mut reachability = PlanReachability::default();
    for job in &plan.jobs {
        let mut reachable = ReachableJob::default();
        match &job.strategy.matrix {
            None | Some(MatrixPlan::Deferred { .. }) => {
                reachable.template =
                    job.skip.is_none() && !condition_is_false(job.condition.as_ref());
                collect_reachable_step_indexes(
                    job.skip.as_ref(),
                    job.condition.as_ref(),
                    &job.steps,
                    &mut reachable.steps,
                );
            }
            Some(MatrixPlan::Static { legs, .. }) => {
                let selected = selected_matrix_legs(job, legs);
                let indexes = if selected.is_empty() && job.matrix_filter.is_some() {
                    (0..job.legs.len()).collect::<Vec<_>>()
                } else {
                    selected
                };
                for index in indexes {
                    if let Some(leg) = job.legs.get(index) {
                        if leg.skip.is_none() && !condition_is_false(leg.condition.as_ref()) {
                            reachable.legs.insert(index);
                        }
                        collect_reachable_step_indexes(
                            leg.skip.as_ref(),
                            leg.condition.as_ref(),
                            &leg.steps,
                            &mut reachable.steps,
                        );
                    }
                }
            }
        }
        if reachable.template || !reachable.legs.is_empty() {
            reachability.jobs.insert(job.id.0.clone(), reachable);
        }
    }
    reachability
}

fn collect_reachable_step_indexes(
    skip: Option<&StaticSkip>,
    condition: Option<&greenlit_engine::Condition>,
    steps: &[StepPlan],
    indexes: &mut BTreeSet<usize>,
) {
    if skip.is_some() || condition_is_false(condition) {
        return;
    }
    indexes.extend(
        steps
            .iter()
            .enumerate()
            .filter(|(_, step)| !condition_is_false(step.condition.as_ref()))
            .map(|(index, _)| index),
    );
}

pub(super) fn selected_matrix_legs(job: &JobPlan, legs: &[MatrixLeg]) -> Vec<usize> {
    let Some(filter) = &job.matrix_filter else {
        return (0..legs.len()).collect();
    };
    let selected = legs
        .iter()
        .enumerate()
        .filter(|(_, leg)| {
            leg.values.len() == filter.len()
                && filter.iter().all(|(key, expected)| {
                    leg.values
                        .get(key.as_str())
                        .is_some_and(|actual| actual == expected)
                })
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if selected.len() == 1 {
        selected
    } else {
        Vec::new()
    }
}

pub(super) fn condition_is_false(condition: Option<&greenlit_engine::Condition>) -> bool {
    condition.is_some_and(|condition| matches!(&condition.eval, PlannedCond::Static(false)))
}

pub(super) fn condition_is_deferred(condition: Option<&greenlit_engine::Condition>) -> bool {
    condition.is_some_and(|condition| matches!(&condition.eval, PlannedCond::Deferred(_)))
}
