//! CLI job/matrix selection and planner-proven workflow reachability.

use std::collections::{HashMap, HashSet, VecDeque};

use greenlit_engine::{ExecutionPlan, JobId, MatrixPlan, MatrixValue};
use greenlit_workflow::model::workflow::Workflow;

use crate::vars;

pub(crate) fn reachable_workflow(
    workflow: &Workflow,
    plan: &ExecutionPlan,
    extraction: &greenlit_workflow::StaticExtraction,
    unresolved: Option<&vars::UnresolvedPlanningVars>,
) -> Workflow {
    let reachability = greenlit_runtime::plan_reachability(plan);
    let unresolved_spans = unresolved.map_or_else(HashSet::new, |error| {
        let mut spans = HashSet::new();
        for name in &error.names {
            for (_, occurrences) in extraction
                .vars
                .iter()
                .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            {
                spans.extend(occurrences.iter().map(ToString::to_string));
            }
        }
        if error.has_dynamic_lookup {
            spans.extend(extraction.dynamic_vars.iter().map(ToString::to_string));
        }
        spans
    });
    let planned_jobs = plan
        .jobs
        .iter()
        .map(|job| (job.id.0.as_str(), job))
        .collect::<HashMap<_, _>>();
    let mut selected = workflow.clone();
    selected.jobs.retain_mut(|job| {
        let Some(job_plan) = planned_jobs.get(job.id.value.as_str()) else {
            return false;
        };
        let promoted = job
            .if_condition
            .as_ref()
            .is_some_and(|condition| unresolved_spans.contains(&condition.span.to_string()));
        if !reachability.job_reachable(&job.id.value) && !promoted {
            return false;
        }
        job.steps = job
            .steps
            .iter()
            .enumerate()
            .filter(|(index, step)| {
                reachability.step_reachable(&job.id.value, *index)
                    || step.if_condition.as_ref().is_some_and(|condition| {
                        unresolved_spans.contains(&condition.span.to_string())
                    })
                    || (promoted && step_can_execute(job_plan, *index))
            })
            .map(|(_, step)| step.clone())
            .collect();
        true
    });
    selected
}

fn step_can_execute(job: &greenlit_engine::JobPlan, index: usize) -> bool {
    let condition_allows = |step: &greenlit_engine::StepPlan| {
        !step.condition.as_ref().is_some_and(|condition| {
            matches!(condition.eval, greenlit_engine::PlannedCond::Static(false))
        })
    };
    job.steps.get(index).is_some_and(condition_allows)
        || job
            .legs
            .iter()
            .filter_map(|leg| leg.steps.get(index))
            .any(condition_allows)
}

pub(crate) fn prune_to_job(
    plan: &ExecutionPlan,
    job: &str,
    selected_matrix: &[(String, String)],
    write_back: bool,
) -> anyhow::Result<ExecutionPlan> {
    let target = JobId(job.to_string());
    if !plan.jobs.iter().any(|candidate| candidate.id == target) {
        anyhow::bail!(
            "no job '{job}' in this workflow\n  fix: pass -j with a job id defined under `jobs:`"
        );
    }
    let mut keep = HashSet::new();
    let mut queue = VecDeque::from([target]);
    while let Some(id) = queue.pop_front() {
        if !keep.insert(id.0.clone()) {
            continue;
        }
        if let Some(job_plan) = plan.jobs.iter().find(|candidate| candidate.id == id) {
            queue.extend(job_plan.needs.iter().cloned());
        }
    }
    let mut pruned = plan.clone();
    pruned
        .jobs
        .retain(|candidate| keep.contains(&candidate.id.0));
    pruned.topo_order.retain(|id| keep.contains(&id.0));
    let selected = parse_matrix_filter(selected_matrix)?;
    let target = pruned
        .jobs
        .iter_mut()
        .find(|candidate| candidate.id.0 == job)
        .ok_or_else(|| anyhow::anyhow!("selected job disappeared while pruning"))?;
    if selected.is_some() && target.strategy.matrix.is_none() {
        anyhow::bail!(
            "job '{job}' is not a matrix job\n  fix: omit `--matrix`, or select a job with `strategy.matrix`"
        );
    }
    if write_back && target.strategy.matrix.is_some() && selected.is_none() {
        anyhow::bail!(
            "`--write-back` needs one exact matrix case for job '{job}'\n  fix: repeat `--matrix KEY=JSON_VALUE` for every property in the desired case"
        );
    }
    if let (Some(filter), Some(MatrixPlan::Static { legs, .. })) =
        (&selected, &target.strategy.matrix)
    {
        let matches = legs
            .iter()
            .filter(|leg| matrix_leg_selected(&leg.values, Some(filter)))
            .count();
        if matches != 1 {
            anyhow::bail!(
                "matrix selection for job '{job}' matched {matches} cases\n  fix: specify every matrix property so exactly one case matches"
            );
        }
    }
    target.matrix_filter = selected;
    Ok(pruned)
}

fn parse_matrix_filter(
    selected: &[(String, String)],
) -> anyhow::Result<Option<indexmap::IndexMap<String, MatrixValue>>> {
    if selected.is_empty() {
        return Ok(None);
    }
    let mut filter = indexmap::IndexMap::new();
    for (key, raw) in selected {
        if filter.contains_key(key) {
            anyhow::bail!(
                "matrix property '{key}' was selected more than once\n  fix: pass each `--matrix KEY=JSON_VALUE` once"
            );
        }
        let value = serde_json::from_str::<serde_json::Value>(raw)
            .unwrap_or_else(|_| serde_json::Value::String(raw.clone()));
        filter.insert(key.clone(), json_matrix_value(&value)?);
    }
    Ok(Some(filter))
}

fn json_matrix_value(value: &serde_json::Value) -> anyhow::Result<MatrixValue> {
    Ok(match value {
        serde_json::Value::Null => MatrixValue::Null,
        serde_json::Value::Bool(value) => MatrixValue::Bool(*value),
        serde_json::Value::Number(value) => MatrixValue::Number(value.as_f64().ok_or_else(|| {
            anyhow::anyhow!(
                "matrix numeric selector is outside the supported range\n  fix: use a finite JSON number"
            )
        })?),
        serde_json::Value::String(value) => MatrixValue::String(value.as_str().into()),
        serde_json::Value::Array(values) => MatrixValue::Sequence(
            values
                .iter()
                .map(json_matrix_value)
                .collect::<anyhow::Result<Vec<_>>>()?
                .into(),
        ),
        serde_json::Value::Object(values) => MatrixValue::Mapping(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), json_matrix_value(value)?)))
                .collect::<anyhow::Result<indexmap::IndexMap<_, _>>>()?
                .into(),
        ),
    })
}

fn matrix_leg_selected(
    values: &indexmap::IndexMap<greenlit_engine::MatrixKey, MatrixValue>,
    filter: Option<&indexmap::IndexMap<String, MatrixValue>>,
) -> bool {
    filter.is_none_or(|filter| {
        values.len() == filter.len()
            && filter.iter().all(|(name, expected)| {
                values
                    .iter()
                    .find(|(key, _)| key.as_str() == name)
                    .is_some_and(|(_, actual)| actual == expected)
            })
    })
}
