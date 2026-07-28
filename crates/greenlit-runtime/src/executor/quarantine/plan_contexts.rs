//! Typed sensitive-context findings retained in a selected execution plan.
//!
//! Partial evaluation records context dependencies as [`DeferReason`] values.
//! Walking those typed reasons avoids reparsing source text and makes the
//! public executor boundary independently fail closed even when a caller does
//! not supply a CLI-authored capability assessment.

use greenlit_engine::{
    CAPABILITY_CREDENTIAL_GITHUB, CAPABILITY_SECRET_CONTEXT, CapabilityFinding, ConcurrencyPlan,
    ContainerPlan, DeferReason, Evaluation, ExecutionPlan, JobOutputsPlan, JobPlan, LegPlan,
    MatrixPlan, Planned, PlannedCond, PlannedValue, RunDefaultsPlan, StepKind, StepPlan,
    StrategyControl,
};

use super::{condition_is_false, plan_reachability, selected_matrix_legs};

pub(super) fn collect(plan: &ExecutionPlan, findings: &mut Vec<CapabilityFinding>) {
    collect_optional_planned(plan.run_name.as_ref(), "workflow.run-name", findings);
    if plan_reachability(plan).any_job_reachable() {
        collect_env(&plan.env, "workflow.env", findings);
        collect_defaults(&plan.defaults, "workflow.defaults.run", findings);
    }
    if let Some(concurrency) = &plan.concurrency {
        collect_concurrency(concurrency, "workflow.concurrency", findings);
    }
    for job in &plan.jobs {
        collect_job(job, findings);
    }
}

fn collect_job(job: &JobPlan, findings: &mut Vec<CapabilityFinding>) {
    let scope = format!("jobs.{}", job.id);
    match &job.strategy.matrix {
        None | Some(MatrixPlan::Deferred { .. }) => {
            if job.skip.is_some() || condition_is_false(job.condition.as_ref()) {
                return;
            }
            collect_strategy(job, &scope, findings);
            collect_job_template(job, &scope, findings);
        }
        Some(MatrixPlan::Static { legs, .. }) => {
            let selected = selected_matrix_legs(job, legs);
            let indexes = if selected.is_empty() && job.matrix_filter.is_some() {
                (0..job.legs.len()).collect::<Vec<_>>()
            } else {
                selected
            };
            let mut reachable = false;
            for index in indexes {
                let Some(leg) = job.legs.get(index) else {
                    continue;
                };
                if leg.skip.is_some() || condition_is_false(leg.condition.as_ref()) {
                    continue;
                }
                reachable = true;
                collect_leg(leg, &format!("{scope}[{index}]"), findings);
            }
            if reachable {
                collect_strategy(job, &scope, findings);
            }
        }
    }
}

fn collect_job_template(job: &JobPlan, scope: &str, findings: &mut Vec<CapabilityFinding>) {
    collect_planned(&job.name, &format!("{scope}.name"), findings);
    collect_optional_planned(job.runner.as_ref(), &format!("{scope}.runs-on"), findings);
    if let Some(container) = &job.container {
        collect_container(container, &format!("{scope}.container"), findings);
    }
    for (name, service) in &job.services {
        collect_container(service, &format!("{scope}.services.{name}"), findings);
    }
    collect_env(&job.env, &format!("{scope}.env"), findings);
    collect_defaults(&job.defaults, &format!("{scope}.defaults.run"), findings);
    if let Some(concurrency) = &job.concurrency {
        collect_concurrency(concurrency, &format!("{scope}.concurrency"), findings);
    }
    collect_optional_condition(job.condition.as_ref(), &format!("{scope}.if"), findings);
    collect_outputs(&job.outputs, &format!("{scope}.outputs"), findings);
    collect_steps(&job.steps, scope, findings);
}

fn collect_leg(leg: &LegPlan, scope: &str, findings: &mut Vec<CapabilityFinding>) {
    collect_planned(&leg.name, &format!("{scope}.name"), findings);
    collect_planned(&leg.runner, &format!("{scope}.runs-on"), findings);
    if let Some(container) = &leg.container {
        collect_container(container, &format!("{scope}.container"), findings);
    }
    for (name, service) in &leg.services {
        collect_container(service, &format!("{scope}.services.{name}"), findings);
    }
    collect_env(&leg.env, &format!("{scope}.env"), findings);
    collect_defaults(&leg.defaults, &format!("{scope}.defaults.run"), findings);
    if let Some(concurrency) = &leg.concurrency {
        collect_concurrency(concurrency, &format!("{scope}.concurrency"), findings);
    }
    collect_optional_condition(leg.condition.as_ref(), &format!("{scope}.if"), findings);
    collect_outputs(&leg.outputs, &format!("{scope}.outputs"), findings);
    collect_steps(&leg.steps, scope, findings);
}

fn collect_strategy(job: &JobPlan, scope: &str, findings: &mut Vec<CapabilityFinding>) {
    if let StrategyControl::Deferred(value) = &job.strategy.fail_fast {
        collect_planned(value, &format!("{scope}.strategy.fail-fast"), findings);
    }
    if let Some(StrategyControl::Deferred(value)) = &job.strategy.max_parallel {
        collect_planned(value, &format!("{scope}.strategy.max-parallel"), findings);
    }
    if let Some(MatrixPlan::Deferred { expressions, .. }) = &job.strategy.matrix {
        for expression in expressions {
            collect_reasons(
                &expression.defers_on,
                &format!("{scope}.strategy.{}", expression.path),
                findings,
            );
        }
    }
}

fn collect_steps(steps: &[StepPlan], job_scope: &str, findings: &mut Vec<CapabilityFinding>) {
    for (index, step) in steps.iter().enumerate() {
        if condition_is_false(step.condition.as_ref()) {
            continue;
        }
        let scope = format!("{job_scope}.steps[{index}]");
        collect_optional_planned(step.name.as_ref(), &format!("{scope}.name"), findings);
        collect_env(&step.env, &format!("{scope}.env"), findings);
        collect_optional_planned(
            step.working_directory.as_ref(),
            &format!("{scope}.working-directory"),
            findings,
        );
        collect_optional_planned(
            step.continue_on_error.as_ref(),
            &format!("{scope}.continue-on-error"),
            findings,
        );
        collect_optional_planned(
            step.timeout_minutes.as_ref(),
            &format!("{scope}.timeout-minutes"),
            findings,
        );
        collect_optional_condition(step.condition.as_ref(), &format!("{scope}.if"), findings);
        match &step.kind {
            StepKind::Run { script, shell } => {
                collect_planned(script, &format!("{scope}.run"), findings);
                collect_optional_planned(shell.as_ref(), &format!("{scope}.shell"), findings);
            }
            StepKind::Uses { with, .. } => {
                collect_env(with, &format!("{scope}.with"), findings);
            }
        }
    }
}

fn collect_container(
    container: &ContainerPlan,
    scope: &str,
    findings: &mut Vec<CapabilityFinding>,
) {
    collect_planned(&container.image, &format!("{scope}.image"), findings);
    if let Some(credentials) = &container.credentials {
        collect_optional_planned(
            credentials.username.as_ref(),
            &format!("{scope}.credentials.username"),
            findings,
        );
        collect_optional_planned(
            credentials.password.as_ref(),
            &format!("{scope}.credentials.password"),
            findings,
        );
    }
    collect_env(&container.env, &format!("{scope}.env"), findings);
    for (index, port) in container.ports.iter().enumerate() {
        collect_planned(port, &format!("{scope}.ports[{index}]"), findings);
    }
    for (index, volume) in container.volumes.iter().enumerate() {
        collect_planned(volume, &format!("{scope}.volumes[{index}]"), findings);
    }
    collect_optional_planned(
        container.options.as_ref(),
        &format!("{scope}.options"),
        findings,
    );
}

fn collect_env<T>(
    values: &indexmap::IndexMap<String, T>,
    scope: &str,
    findings: &mut Vec<CapabilityFinding>,
) where
    T: std::borrow::Borrow<Planned<String>>,
{
    for (name, value) in values {
        collect_planned(value.borrow(), &format!("{scope}.{name}"), findings);
    }
}

fn collect_defaults(
    defaults: &RunDefaultsPlan,
    scope: &str,
    findings: &mut Vec<CapabilityFinding>,
) {
    collect_optional_planned(defaults.shell.as_ref(), &format!("{scope}.shell"), findings);
    collect_optional_planned(
        defaults.working_directory.as_ref(),
        &format!("{scope}.working-directory"),
        findings,
    );
}

fn collect_concurrency(
    concurrency: &ConcurrencyPlan,
    scope: &str,
    findings: &mut Vec<CapabilityFinding>,
) {
    collect_planned(&concurrency.group, &format!("{scope}.group"), findings);
    collect_planned(
        &concurrency.cancel_in_progress,
        &format!("{scope}.cancel-in-progress"),
        findings,
    );
}

fn collect_outputs(outputs: &JobOutputsPlan, scope: &str, findings: &mut Vec<CapabilityFinding>) {
    for (name, output) in &outputs.entries {
        if let PlannedValue::Deferred(deferred) = &output.value {
            collect_reasons(&deferred.defers_on, &format!("{scope}.{name}"), findings);
        }
    }
}

fn collect_optional_condition(
    condition: Option<&greenlit_engine::Condition>,
    scope: &str,
    findings: &mut Vec<CapabilityFinding>,
) {
    if let Some(condition) = condition
        && let PlannedCond::Deferred(deferred) = &condition.eval
    {
        collect_reasons(&deferred.defers_on, scope, findings);
    }
}

fn collect_optional_planned<T>(
    value: Option<&Planned<T>>,
    scope: &str,
    findings: &mut Vec<CapabilityFinding>,
) {
    if let Some(value) = value {
        collect_planned(value, scope, findings);
    }
}

fn collect_planned<T>(value: &Planned<T>, scope: &str, findings: &mut Vec<CapabilityFinding>) {
    if let Evaluation::Deferred(deferred) = &value.evaluation {
        collect_reasons(&deferred.defers_on, scope, findings);
    }
}

fn collect_reasons(reasons: &[DeferReason], scope: &str, findings: &mut Vec<CapabilityFinding>) {
    if reasons
        .iter()
        .any(|reason| matches!(reason, DeferReason::SecretsContext))
    {
        findings.push(CapabilityFinding::new(
            CAPABILITY_SECRET_CONTEXT,
            scope,
            "the reachable field requires the workflow secrets context",
        ));
    }
    if reasons.iter().any(github_reason_can_expose_token) {
        findings.push(CapabilityFinding::new(
            CAPABILITY_CREDENTIAL_GITHUB,
            scope,
            "the reachable field requires `github.token` or a whole/dynamically selected GitHub context that can expose it",
        ));
    }
}

fn github_reason_can_expose_token(reason: &DeferReason) -> bool {
    match reason {
        DeferReason::GithubContext { property: None } => true,
        DeferReason::GithubContext {
            property: Some(property),
        } => property.eq_ignore_ascii_case("token"),
        _ => false,
    }
}
