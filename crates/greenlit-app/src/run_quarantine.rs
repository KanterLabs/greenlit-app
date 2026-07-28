//! Selected-plan stabilization quarantine before sensitive preparation.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use greenlit_engine::{
    CAPABILITY_CREDENTIAL_GITHUB, CAPABILITY_DISPATCH_INPUT, CAPABILITY_SECRET_CONTEXT,
    CAPABILITY_SOURCE_WRITE_BACK, CAPABILITY_VARIABLE_CONTEXT, CapabilityFinding,
    DEFAULT_MAX_MATRIX_LEGS, ExecutionPlan, PlanOptions, QuarantineDecision, QuarantineOutcome,
    build_synthetic_event, decide_capability_quarantine, plan,
};
use greenlit_expr::Value;
use greenlit_workflow::model::workflow::Workflow;

use crate::cli::RunArgs;
use crate::{errors, secrets, vars};

pub(crate) struct PreparedQuarantine {
    pub(crate) workflow: Workflow,
    pub(crate) git: greenlit_engine::git::GitContext,
    pub(crate) event: greenlit_engine::SyntheticEvent,
    pub(crate) plan: ExecutionPlan,
    pub(crate) vars: Value,
    pub(crate) assessment: greenlit_runtime::RuntimeCapabilityAssessment,
}

pub(crate) fn explicit_sensitive_values(args: &RunArgs) -> Vec<String> {
    // Phase 12 must not resolve the secret context or retrieve stored
    // credentials before quarantine. Explicit secret values still enter the
    // in-memory masking/scanning registry even when the selected workflow
    // cannot consume them. Explicit inputs and variables are rejected before
    // this capture path; Phase 16 owns their credential-bearing preflight.
    args.secrets
        .iter()
        .map(|(_, value)| value.clone())
        .collect()
}

pub(crate) fn reject_explicit_dispatch_inputs(
    inputs: &[(String, String)],
    allow_degraded: bool,
) -> anyhow::Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let findings = [CapabilityFinding::new(
        CAPABILITY_DISPATCH_INPUT,
        "command-line.--input",
        "explicit dispatch inputs have not completed credential-bearing input preflight",
    )];
    let decision = decide_capability_quarantine(&findings, allow_degraded);
    reject_decision(&decision)
}

pub(crate) fn reject_explicit_variables(
    variables: &[(String, String)],
    allow_degraded: bool,
) -> anyhow::Result<()> {
    if variables.is_empty() {
        return Ok(());
    }
    let findings = [CapabilityFinding::new(
        CAPABILITY_VARIABLE_CONTEXT,
        "command-line.--var",
        "explicit variables have not completed credential-bearing input preflight",
    )];
    let decision = decide_capability_quarantine(&findings, allow_degraded);
    reject_decision(&decision)
}

pub(crate) fn reject_vars_context(
    extraction: &greenlit_workflow::StaticExtraction,
    allow_degraded: bool,
) -> anyhow::Result<()> {
    let Some(finding) = variable_context_finding(extraction) else {
        return Ok(());
    };
    let findings = [finding];
    let decision = decide_capability_quarantine(&findings, allow_degraded);
    reject_decision(&decision)
}

pub(crate) fn conservative_planning_variables(
    extraction: &greenlit_workflow::StaticExtraction,
) -> (Value, Option<vars::UnresolvedPlanningVars>) {
    let mut placeholders = BTreeMap::new();
    for name in extraction.vars.keys() {
        placeholders.insert(name.to_ascii_uppercase(), Value::String(String::new()));
    }
    if extraction.has_dynamic_vars_lookup {
        for event_name in ["PUSH", "PULL_REQUEST", "WORKFLOW_DISPATCH"] {
            placeholders
                .entry(event_name.to_string())
                .or_insert_with(|| Value::String(String::new()));
        }
    }
    let values = Value::object(placeholders.into_iter().collect());
    if extraction.vars.is_empty() && !extraction.has_dynamic_vars_lookup {
        return (values, None);
    }
    let unresolved = vars::UnresolvedPlanningVars {
        names: extraction.vars.keys().cloned().collect(),
        has_dynamic_lookup: extraction.has_dynamic_vars_lookup,
    };
    (values, Some(unresolved))
}

pub(crate) fn prepare(
    workflow: Workflow,
    authored_extraction: greenlit_workflow::StaticExtraction,
    source_root: &Path,
    args: &RunArgs,
) -> anyhow::Result<PreparedQuarantine> {
    greenlit_engine::validate_v0_support(&workflow).map_err(|error| errors::plan_error(&error))?;
    let (planning_vars, unresolved) = conservative_planning_variables(&authored_extraction);

    let git = greenlit_engine::git::collect_git_context(source_root)
        .map_err(|error| errors::event_error(&greenlit_engine::EventError::Git(error)))?;
    let event_kind: greenlit_engine::EventKind = args.event.into();
    let dispatch_inputs: HashMap<String, String> = args.inputs.iter().cloned().collect();
    let mut event = build_synthetic_event(event_kind, source_root, &workflow, &dispatch_inputs)
        .map_err(|error| errors::event_error(&error))?;
    if references_github_token(&authored_extraction) {
        event.deferred_github_properties.insert("token".to_string());
    }

    let planned = plan(
        &workflow,
        &event,
        &PlanOptions {
            vars: planning_vars.clone(),
            max_matrix_legs: DEFAULT_MAX_MATRIX_LEGS,
        },
    )
    .map_err(|error| errors::plan_error(&error))?;
    let selected_plan = match &args.job {
        Some(job) => {
            crate::run_selection::prune_to_job(&planned, job, &args.matrix, args.write_back)?
        }
        None => planned,
    };
    let reachable = crate::run_selection::reachable_workflow(
        &workflow,
        &selected_plan,
        &authored_extraction,
        unresolved.as_ref(),
    );
    let extraction = greenlit_workflow::extract_static(&reachable)
        .map_err(|error| errors::parse_error(&error))?;
    // Variable resolution remains quarantined until its trust and input
    // preflight is certified. Keep the runtime context empty and carry the
    // reachable source fact into the authoritative assessment below so a
    // `run` invocation retains a `blocked` terminal result instead of
    // misclassifying the policy decision as a preparation failure.
    let resolved_vars = Value::object(Vec::<(String, Value)>::new());

    let additional = source_findings(&extraction, args);
    let empty_secrets = Value::object(Vec::<(String, Value)>::new());
    let inputs = greenlit_runtime::RuntimeCapabilityInputs::new(
        &event.github,
        &resolved_vars,
        &event.inputs,
        &empty_secrets,
        None,
        false,
        args.write_back,
    );
    // The runtime assessment walks the selected plan's typed `DeferReason`
    // inventory, so computed indexes, wildcards, and whole-context
    // `secrets`/`github` references cannot disappear through this source-only
    // extraction layer. `additional` retains source facts that are not
    // represented in the execution plan.
    let assessment = greenlit_runtime::assess_runtime_capabilities(
        &selected_plan,
        &inputs,
        &additional,
        args.allow_degraded,
    );
    Ok(PreparedQuarantine {
        workflow,
        git,
        event,
        plan: selected_plan,
        vars: resolved_vars,
        assessment,
    })
}

pub(crate) fn reject_blocked(
    assessment: &greenlit_runtime::RuntimeCapabilityAssessment,
) -> anyhow::Result<()> {
    reject_decision(assessment.decision())
}

fn reject_decision(decision: &QuarantineDecision) -> anyhow::Result<()> {
    if decision.outcome() != QuarantineOutcome::Blocked {
        return Ok(());
    }
    let blocked = decision.blocking_findings().first().ok_or_else(|| {
        anyhow::anyhow!(
            "stabilization quarantine produced an empty blocked decision\n  fix: update Greenlit, then retry"
        )
    })?;
    let finding = blocked.finding();
    let owner = blocked.owning_phase().map_or_else(
        || "an unassigned stabilization phase".to_string(),
        |phase| format!("stabilization Phase {phase}"),
    );
    anyhow::bail!(
        "uncertified capability `{}` at `{}` is blocked before daemon, credential, network, action, or container work: {} ({owner})\n  fix: {}",
        finding.capability_id(),
        finding.scope(),
        finding.reason(),
        blocked.user_action()
    )
}

pub(crate) fn render_degraded(
    assessment: &greenlit_runtime::RuntimeCapabilityAssessment,
    masker: &greenlit_engine::execution::Masker,
) -> anyhow::Result<()> {
    let decision = assessment.decision();
    if decision.outcome() != QuarantineOutcome::Degraded {
        return Ok(());
    }
    let mut rendered = format!(
        "warning: `--allow-degraded` forced {} uncertified capability finding(s); compatibility is degraded and assurance is none",
        decision.forced_findings().len()
    );
    for resolved in decision.forced_findings() {
        let finding = resolved.finding();
        rendered.push_str(&format!(
            "\n  forced: {} at {} — {}",
            finding.capability_id(),
            finding.scope(),
            finding.reason()
        ));
    }
    rendered.push('\n');
    let stderr = std::io::stderr();
    crate::render::terminal::write_sanitized(&mut stderr.lock(), &masker.apply(&rendered))
        .map_err(|error| {
            anyhow::anyhow!(
                "could not render the degraded-run warning: {error}\n  fix: ensure standard error is writable, then retry"
            )
        })
}

pub(crate) fn evidence_findings(
    assessment: &greenlit_runtime::RuntimeCapabilityAssessment,
) -> Vec<greenlit_engine::FeatureFinding> {
    let decision = assessment.decision();
    decision
        .forced_findings()
        .iter()
        .map(|resolved| {
            let finding = resolved.finding();
            greenlit_engine::FeatureFinding {
                code: finding.capability_id().to_string(),
                disposition: greenlit_engine::FindingDisposition::Degraded,
                scope: finding.scope().to_string(),
                reason: finding.reason().to_string(),
            }
        })
        .chain(decision.blocking_findings().iter().map(|resolved| {
            let finding = resolved.finding();
            greenlit_engine::FeatureFinding {
                code: finding.capability_id().to_string(),
                disposition: greenlit_engine::FindingDisposition::Unsupported,
                scope: finding.scope().to_string(),
                reason: finding.reason().to_string(),
            }
        }))
        .collect()
}

fn source_findings(
    extraction: &greenlit_workflow::StaticExtraction,
    args: &RunArgs,
) -> Vec<CapabilityFinding> {
    let mut findings = Vec::new();
    if let Some(finding) = variable_context_finding(extraction) {
        findings.push(finding);
    }
    for (name, spans) in &extraction.secrets {
        for span in spans {
            findings.push(CapabilityFinding::new(
                CAPABILITY_SECRET_CONTEXT,
                span.to_string(),
                format!("the selected workflow references `secrets.{name}`"),
            ));
        }
    }
    if references_github_token(extraction) {
        findings.push(CapabilityFinding::new(
            CAPABILITY_CREDENTIAL_GITHUB,
            "github.token",
            "the selected workflow requires a GitHub bearer token",
        ));
    }
    if args.write_back {
        findings.push(CapabilityFinding::new(
            CAPABILITY_SOURCE_WRITE_BACK,
            "run.write-back",
            "write-back would apply untrusted sandbox changes to the host worktree",
        ));
    }
    findings
}

fn variable_context_finding(
    extraction: &greenlit_workflow::StaticExtraction,
) -> Option<CapabilityFinding> {
    if extraction.vars.is_empty() && !extraction.has_dynamic_vars_lookup {
        return None;
    }
    let scope = extraction
        .vars
        .values()
        .flatten()
        .next()
        .or_else(|| extraction.dynamic_vars.first())
        .map_or_else(|| "workflow.vars".to_string(), ToString::to_string);
    Some(CapabilityFinding::new(
        CAPABILITY_VARIABLE_CONTEXT,
        scope,
        "the workflow's `vars` context use has not completed trust and input preflight",
    ))
}

fn references_github_token(extraction: &greenlit_workflow::StaticExtraction) -> bool {
    extraction.references_github_token
        || extraction.secrets.contains_key(secrets::GITHUB_TOKEN_NAME)
}
