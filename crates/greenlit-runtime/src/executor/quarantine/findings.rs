use indexmap::IndexMap;

use greenlit_engine::{
    CAPABILITY_ACTION_USES, CAPABILITY_EXECUTION_SHELL, CAPABILITY_REACHABILITY_AMBIGUOUS,
    CAPABILITY_SECURITY_BOUNDARY, CAPABILITY_SERVICE_CONTAINER, CapabilityFinding, ContainerPlan,
    EnvValue, Evaluation, JobPlan, MatrixPlan, StaticSkip, StepKind, StepPlan,
};

use super::{condition_is_deferred, condition_is_false, selected_matrix_legs};

pub(super) fn collect_job_findings(job: &JobPlan, findings: &mut Vec<CapabilityFinding>) {
    let scope = format!("jobs.{}", job.id);
    match &job.strategy.matrix {
        None => collect_execution_shape(
            ExecutionShape {
                scope: &scope,
                skip: job.skip.as_ref(),
                condition: job.condition.as_ref(),
                container: job.container.as_ref(),
                services: &job.services,
                steps: &job.steps,
                matrix_is_deferred: false,
            },
            findings,
        ),
        Some(MatrixPlan::Deferred { .. }) => collect_execution_shape(
            ExecutionShape {
                scope: &scope,
                skip: job.skip.as_ref(),
                condition: job.condition.as_ref(),
                container: job.container.as_ref(),
                services: &job.services,
                steps: &job.steps,
                matrix_is_deferred: true,
            },
            findings,
        ),
        Some(MatrixPlan::Static { legs, .. }) => {
            if legs.len() != job.legs.len() {
                findings.push(CapabilityFinding::new(
                    CAPABILITY_SECURITY_BOUNDARY,
                    scope,
                    "the static matrix and its execution legs have inconsistent shapes",
                ));
                return;
            }
            let selected = selected_matrix_legs(job, legs);
            if selected.is_empty() && job.matrix_filter.is_some() {
                findings.push(CapabilityFinding::new(
                    CAPABILITY_REACHABILITY_AMBIGUOUS,
                    scope,
                    "the requested matrix selection is not exactly provable",
                ));
                return;
            }
            for index in selected {
                let leg = &job.legs[index];
                collect_execution_shape(
                    ExecutionShape {
                        scope: &format!("jobs.{}[{index}]", job.id),
                        skip: leg.skip.as_ref(),
                        condition: leg.condition.as_ref(),
                        container: leg.container.as_ref(),
                        services: &leg.services,
                        steps: &leg.steps,
                        matrix_is_deferred: false,
                    },
                    findings,
                );
            }
        }
    }
}

struct ExecutionShape<'a> {
    scope: &'a str,
    skip: Option<&'a StaticSkip>,
    condition: Option<&'a greenlit_engine::Condition>,
    container: Option<&'a ContainerPlan>,
    services: &'a IndexMap<String, ContainerPlan>,
    steps: &'a [StepPlan],
    matrix_is_deferred: bool,
}

fn collect_execution_shape(shape: ExecutionShape<'_>, findings: &mut Vec<CapabilityFinding>) {
    if shape.skip.is_some() || condition_is_false(shape.condition) {
        return;
    }

    if let Some(container) = shape.container {
        collect_container_findings(shape.scope, "container", container, findings);
    }
    for (service, container) in shape.services {
        let service_scope = format!("{}.services.{service}", shape.scope);
        findings.push(CapabilityFinding::new(
            CAPABILITY_SERVICE_CONTAINER,
            &service_scope,
            "the reachable job declares a service container",
        ));
        collect_container_findings(&service_scope, "service", container, findings);
    }
    for (index, step) in shape.steps.iter().enumerate() {
        if condition_is_false(step.condition.as_ref()) {
            continue;
        }
        let step_scope = format!("{}.steps[{index}]", shape.scope);
        match &step.kind {
            StepKind::Run { .. } => {
                findings.push(CapabilityFinding::new(
                    CAPABILITY_EXECUTION_SHELL,
                    &step_scope,
                    "the reachable step executes a shell script",
                ));
            }
            StepKind::Uses { .. } => findings.push(CapabilityFinding::new(
                CAPABILITY_ACTION_USES,
                &step_scope,
                "the reachable step executes an action",
            )),
        }
        if condition_is_deferred(step.condition.as_ref()) {
            findings.push(CapabilityFinding::new(
                CAPABILITY_REACHABILITY_AMBIGUOUS,
                step_scope,
                "the step's reachability remains dynamic",
            ));
        }
    }
    if shape.matrix_is_deferred || condition_is_deferred(shape.condition) {
        findings.push(CapabilityFinding::new(
            CAPABILITY_REACHABILITY_AMBIGUOUS,
            shape.scope,
            "the job's reachability or matrix expansion remains dynamic",
        ));
    }
}

fn collect_container_findings(
    scope: &str,
    kind: &str,
    container: &ContainerPlan,
    findings: &mut Vec<CapabilityFinding>,
) {
    if container.credentials.is_some() {
        findings.push(CapabilityFinding::new(
            CAPABILITY_SECURITY_BOUNDARY,
            format!("{scope}.credentials"),
            format!("the {kind} image pull requires registry credentials"),
        ));
    }
    if container
        .options
        .as_ref()
        .is_some_and(planned_options_are_security_ambiguous)
        || container
            .volumes
            .iter()
            .any(planned_volume_is_security_ambiguous)
    {
        findings.push(CapabilityFinding::new(
            CAPABILITY_SECURITY_BOUNDARY,
            scope,
            format!("the {kind} request has options or volumes that are not statically safe"),
        ));
    }
}

fn planned_options_are_security_ambiguous(value: &EnvValue) -> bool {
    match &value.evaluation {
        Evaluation::Deferred(_) => true,
        Evaluation::Static(value) => {
            let lower = value.to_ascii_lowercase();
            [
                "--privileged",
                "--network",
                "--net",
                "--pid",
                "--ipc",
                "--volume",
                "--mount",
                "--cap-add",
                "--device",
                "--security-opt",
                "--userns",
            ]
            .iter()
            .any(|flag| lower.contains(flag))
                || lower
                    .split_whitespace()
                    .any(|token| token == "-v" || token.starts_with("-v="))
        }
    }
}

fn planned_volume_is_security_ambiguous(value: &EnvValue) -> bool {
    match &value.evaluation {
        Evaluation::Deferred(_) => true,
        Evaluation::Static(value) => {
            let mut parts = value.split(':');
            let source = parts.next();
            let has_destination = parts.next().is_some();
            has_destination
                && source.is_some_and(|source| {
                    source.starts_with('/')
                        || source.starts_with("./")
                        || source.starts_with("../")
                        || source.starts_with("~/")
                })
        }
    }
}
