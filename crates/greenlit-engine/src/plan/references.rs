//! Validation of runtime `needs.*` output declarations and matrix-output
//! collision lints.

use greenlit_workflow::extract::NeedsOutputReference;
use greenlit_workflow::model::workflow::Workflow;
use std::collections::{HashMap, HashSet};

use crate::condition::Condition;
use crate::defer::DeferReason;
use crate::graph::{JobGraph, JobId};
use crate::lints::Lint;
use crate::outputs::JobOutputsPlan;
use crate::partial_eval::DeclaredOutputs;
use crate::pass_through::ContainerPlan;
use crate::planned::{Evaluation, Planned};

use super::{JobPlan, LegPlan, RunDefaultsPlan, StepKind, StepPlan};

struct IndexedJobReferences<'a> {
    output_names: DeclaredOutputs,
    needs_outputs: Vec<&'a NeedsOutputReference>,
}

/// Declaration-order lookup data shared by every job-planning pass.
///
/// The parsed workflow and static extraction are both declaration ordered,
/// so one compact index removes repeated full-workflow scans without
/// changing any emitted order.
pub(super) struct WorkflowReferenceIndex<'a> {
    jobs: Vec<IndexedJobReferences<'a>>,
}

impl<'a> WorkflowReferenceIndex<'a> {
    pub(super) fn new(workflow: &Workflow, references: &'a [NeedsOutputReference]) -> Self {
        let mut by_id = HashMap::with_capacity(workflow.jobs.len());
        let mut jobs = Vec::with_capacity(workflow.jobs.len());
        for (index, job) in workflow.jobs.iter().enumerate() {
            by_id.insert(job.id.value.as_str(), index);
            let output_names = DeclaredOutputs::new(
                job.outputs
                    .iter()
                    .map(|(name, _)| name.value.clone())
                    .collect(),
            );
            jobs.push(IndexedJobReferences {
                output_names,
                needs_outputs: Vec::new(),
            });
        }
        for reference in references {
            if let Some(index) = by_id.get(reference.referencing_job.as_str())
                && let Some(job) = jobs.get_mut(*index)
            {
                job.needs_outputs.push(reference);
            }
        }
        Self { jobs }
    }

    pub(super) fn output_names(&self, job_index: usize) -> Option<DeclaredOutputs> {
        self.jobs.get(job_index).map(|job| job.output_names.clone())
    }

    fn declares_output(&self, job_index: usize, output: &str) -> bool {
        self.jobs
            .get(job_index)
            .is_some_and(|job| job.output_names.contains(output))
    }

    fn needs_outputs(&self, job_index: usize) -> &[&'a NeedsOutputReference] {
        self.jobs
            .get(job_index)
            .map_or(&[], |job| job.needs_outputs.as_slice())
    }
}

/// Emits a warning when a field anywhere in a job references an output its
/// direct dependency never declares. GitHub accepts that lookup and yields
/// an empty string, so it is not an error. A reference to a job outside the
/// direct `needs:` map follows the same missing-property rule and receives
/// no undeclared-output lint.
///
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#needs-context>
/// <https://docs.github.com/en/actions/learn-github-actions/contexts#available-contexts>
pub(super) fn lint_needs_output_references(
    referencing: &ReferencingJob<'_>,
    job_index: usize,
    references: &WorkflowReferenceIndex<'_>,
    graph: &JobGraph,
    lints: &mut Vec<Lint>,
) {
    let direct_dependencies: HashMap<String, &JobId> = referencing
        .needs
        .iter()
        .map(|dependency| {
            (
                greenlit_expr::value::ordinal_ignore_case_key(&dependency.0),
                dependency,
            )
        })
        .collect();
    let mut seen = HashSet::new();
    for reference in references.needs_outputs(job_index) {
        let Some(direct_dependency) = direct_dependencies
            .get(&greenlit_expr::value::ordinal_ignore_case_key(
                &reference.referenced_job,
            ))
            .copied()
        else {
            continue;
        };
        let declares_it = graph
            .idx_of(direct_dependency)
            .is_some_and(|dependency_index| {
                references.declares_output(dependency_index.0 as usize, &reference.output)
            });
        let occurrence = (
            greenlit_expr::value::ordinal_ignore_case_key(&direct_dependency.0),
            greenlit_expr::value::ordinal_ignore_case_key(&reference.output),
            reference.span.clone(),
        );
        if !declares_it && seen.insert(occurrence) {
            lints.push(Lint::undeclared_needed_output(
                reference.span.clone(),
                &direct_dependency.0,
                &reference.output,
            ));
        }
    }
}

/// The referencing job's direct dependencies, for
/// [`lint_needs_output_references`].
pub(super) struct ReferencingJob<'a> {
    pub(super) needs: &'a [JobId],
}

fn collect_needs_producers(reasons: &[DeferReason], out: &mut HashSet<JobId>) {
    for reason in reasons {
        if let DeferReason::NeedsOutput { job, .. } = reason {
            out.insert(job.clone());
        }
    }
}

fn collect_condition_reasons(c: &Condition, out: &mut HashSet<JobId>) {
    if let crate::condition::PlannedCond::Deferred(d) = &c.eval {
        collect_needs_producers(&d.defers_on, out);
    }
}

fn collect_planned_reasons<T>(value: &Planned<T>, out: &mut HashSet<JobId>) {
    if let Evaluation::Deferred(deferred) = &value.evaluation {
        collect_needs_producers(&deferred.defers_on, out);
    }
}

fn collect_optional_planned_reasons<T>(value: Option<&Planned<T>>, out: &mut HashSet<JobId>) {
    if let Some(value) = value {
        collect_planned_reasons(value, out);
    }
}

fn collect_env_reasons<T>(values: impl IntoIterator<Item = T>, out: &mut HashSet<JobId>)
where
    T: std::borrow::Borrow<Planned<String>>,
{
    for value in values {
        collect_planned_reasons(value.borrow(), out);
    }
}

fn collect_defaults_reasons(defaults: &RunDefaultsPlan, out: &mut HashSet<JobId>) {
    collect_optional_planned_reasons(defaults.shell.as_ref(), out);
    collect_optional_planned_reasons(defaults.working_directory.as_ref(), out);
}

fn collect_container_reasons(container: &ContainerPlan, out: &mut HashSet<JobId>) {
    collect_planned_reasons(&container.image, out);
    if let Some(credentials) = &container.credentials {
        collect_optional_planned_reasons(credentials.username.as_ref(), out);
        collect_optional_planned_reasons(credentials.password.as_ref(), out);
    }
    collect_env_reasons(container.env.values(), out);
    collect_env_reasons(&container.ports, out);
    collect_env_reasons(&container.volumes, out);
    collect_optional_planned_reasons(container.options.as_ref(), out);
}

fn collect_job_reasons(job: &JobPlan, out: &mut HashSet<JobId>) {
    collect_planned_reasons(&job.name, out);
    collect_optional_planned_reasons(job.runner.as_ref(), out);
    collect_strategy_reasons(&job.strategy, out);
    if let Some(container) = &job.container {
        collect_container_reasons(container, out);
    }
    for service in job.services.values() {
        collect_container_reasons(service, out);
    }
    collect_env_reasons(job.env.values(), out);
    collect_defaults_reasons(&job.defaults, out);
    if let Some(condition) = &job.condition {
        collect_condition_reasons(condition, out);
    }
    collect_step_reasons(&job.steps, out);
    collect_output_reasons(&job.outputs, out);
    for leg in &job.legs {
        collect_leg_reasons(leg, out);
    }
}

fn collect_leg_reasons(leg: &LegPlan, out: &mut HashSet<JobId>) {
    collect_planned_reasons(&leg.name, out);
    collect_planned_reasons(&leg.runner, out);
    if let Some(container) = &leg.container {
        collect_container_reasons(container, out);
    }
    for service in leg.services.values() {
        collect_container_reasons(service, out);
    }
    collect_env_reasons(leg.env.values(), out);
    collect_defaults_reasons(&leg.defaults, out);
    if let Some(condition) = &leg.condition {
        collect_condition_reasons(condition, out);
    }
    collect_step_reasons(&leg.steps, out);
    collect_output_reasons(&leg.outputs, out);
}

fn collect_strategy_reasons(strategy: &crate::matrix::StrategyPlan, out: &mut HashSet<JobId>) {
    if let crate::matrix::StrategyControl::Deferred(value) = &strategy.fail_fast {
        collect_planned_reasons(value, out);
    }
    if let Some(crate::matrix::StrategyControl::Deferred(value)) = &strategy.max_parallel {
        collect_planned_reasons(value, out);
    }
    if let Some(crate::matrix::MatrixPlan::Deferred { expressions, .. }) = &strategy.matrix {
        for expression in expressions {
            collect_needs_producers(&expression.defers_on, out);
        }
    }
}

fn collect_step_reasons(steps: &[StepPlan], out: &mut HashSet<JobId>) {
    for step in steps {
        collect_optional_planned_reasons(step.name.as_ref(), out);
        collect_env_reasons(step.env.values(), out);
        collect_optional_planned_reasons(step.working_directory.as_ref(), out);
        collect_optional_planned_reasons(step.continue_on_error.as_ref(), out);
        collect_optional_planned_reasons(step.timeout_minutes.as_ref(), out);
        if let Some(condition) = &step.condition {
            collect_condition_reasons(condition, out);
        }
        match &step.kind {
            StepKind::Run { script, shell } => {
                collect_planned_reasons(script, out);
                collect_optional_planned_reasons(shell.as_ref(), out);
            }
            StepKind::Uses { with, .. } => collect_env_reasons(with.values(), out),
        }
    }
}

fn collect_output_reasons(outputs: &JobOutputsPlan, out: &mut HashSet<JobId>) {
    for output in outputs.entries.values() {
        if let crate::outputs::PlannedValue::Deferred(deferred) = &output.value {
            collect_needs_producers(&deferred.defers_on, out);
        }
    }
}

/// A matrix job's output map is shared across all its legs (the last leg to finish always
/// wins — a well-known GHA limitation); warn when such a job's outputs are
/// actually read by a dependent, since that dependent's result is then
/// effectively nondeterministic across parallel legs.
pub(crate) fn lint_matrix_output_collisions(jobs: &[JobPlan], lints: &mut Vec<Lint>) {
    // Walk every retained field once. The former producer-first loop rebuilt
    // a dependent's complete deferred-reason set for every matrix producer,
    // multiplying work by both the job count and plan payload size.
    let mut referenced_direct_producers = HashSet::new();
    for dependent in jobs {
        let direct_needs: HashSet<&str> =
            dependent.needs.iter().map(|need| need.0.as_str()).collect();
        let mut referenced_producers = HashSet::new();
        collect_job_reasons(dependent, &mut referenced_producers);
        referenced_direct_producers.extend(
            referenced_producers
                .into_iter()
                .filter(|producer| direct_needs.contains(producer.0.as_str())),
        );
    }

    for producer in jobs {
        if producer.legs.len() <= 1
            || !producer
                .legs
                .iter()
                .any(|leg| !leg.outputs.entries.is_empty())
        {
            continue;
        }
        if referenced_direct_producers.contains(&producer.id) {
            lints.push(Lint::matrix_outputs_collision(
                producer.span.clone(),
                &producer.id.0,
            ));
        }
    }
}
