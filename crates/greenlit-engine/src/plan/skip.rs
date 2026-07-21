//! Static-skip propagation (design memo §3.3): once every [`super::JobPlan`]
//! exists, decide each job's (and matrix leg's) [`super::StaticSkip`],
//! processed over the `needs` graph's topological order so a dependency's
//! "fully skipped" status is always known before its dependents are
//! processed.

use std::collections::HashMap;

use crate::condition::{Condition, PlannedCond};
use crate::graph::{JobGraph, JobId};

use super::{JobPlan, StaticSkip};

pub(crate) fn propagate_static_skip(graph: &JobGraph, jobs: &mut [JobPlan]) {
    let mut by_id: HashMap<JobId, usize> = HashMap::new();
    for (i, j) in jobs.iter().enumerate() {
        by_id.insert(j.id.clone(), i);
    }
    let mut fully_skipped: HashMap<JobId, bool> = HashMap::new();

    for idx in graph.topo_order() {
        let id = graph.id_of(*idx).clone();
        let Some(&i) = by_id.get(&id) else { continue };

        let first_skipped_need = jobs[i]
            .needs
            .iter()
            .find(|need| fully_skipped.get(*need).copied().unwrap_or(false))
            .cloned();

        if !jobs[i].strategy.is_matrix {
            let skip = decide_skip(
                jobs[i].condition.as_ref(),
                jobs[i].implicit_status_gate,
                &first_skipped_need,
            );
            jobs[i].skip = skip;
            fully_skipped.insert(id, jobs[i].skip.is_some());
        } else {
            let implicit_gate = jobs[i].implicit_status_gate;
            let mut all_skipped = true;
            for leg in &mut jobs[i].legs {
                let skip = decide_skip(leg.condition.as_ref(), implicit_gate, &first_skipped_need);
                all_skipped &= skip.is_some();
                leg.skip = skip;
            }
            fully_skipped.insert(id, all_skipped && !jobs[i].legs.is_empty());
        }
    }
}

fn decide_skip(
    condition: Option<&Condition>,
    implicit_status_gate: bool,
    first_skipped_need: &Option<JobId>,
) -> Option<StaticSkip> {
    if let Some(c) = condition
        && let PlannedCond::Static(false) = c.eval
    {
        return Some(StaticSkip::ConditionFalse);
    }
    if implicit_status_gate && let Some(need) = first_skipped_need {
        return Some(StaticSkip::NeedSkipped { need: need.clone() });
    }
    None
}
