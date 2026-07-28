//! Selected-plan stabilization quarantine before sensitive preparation.

use std::collections::HashMap;
use std::path::Path;

use greenlit_engine::{
    CAPABILITY_CREDENTIAL_GITHUB, CAPABILITY_REACHABILITY_AMBIGUOUS, CAPABILITY_SECRET_CONTEXT,
    CAPABILITY_SOURCE_WRITE_BACK, CAPABILITY_VARIABLE_REMOTE, CapabilityFinding,
    DEFAULT_MAX_MATRIX_LEGS, ExecutionPlan, PlanOptions, QuarantineOutcome, build_synthetic_event,
    plan,
};
use greenlit_expr::Value;
use greenlit_workflow::model::workflow::Workflow;

use crate::cli::RunArgs;
use crate::{errors, secrets, vars};

pub(crate) struct LocalVariables {
    dotenv: Option<Vec<(String, String)>>,
}

impl LocalVariables {
    pub(crate) fn read(repo_root: &Path) -> anyhow::Result<Self> {
        vars::read_dotenv_vars(repo_root)
            .map(|dotenv| Self { dotenv })
            .map_err(|message| anyhow::anyhow!(message))
    }

    fn entries(&self) -> &[(String, String)] {
        self.dotenv.as_deref().unwrap_or_default()
    }

    fn exists(&self) -> bool {
        self.dotenv.is_some()
    }
}

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
    // credentials before quarantine. Values the user already supplied on the
    // command line still have to enter the in-memory masking/scanning
    // registry, even when the selected workflow cannot consume them.
    args.secrets
        .iter()
        .map(|(_, value)| value.clone())
        .collect()
}

pub(crate) fn prepare(
    workflow: Workflow,
    source_root: &Path,
    args: &RunArgs,
    local_variables: &LocalVariables,
) -> anyhow::Result<PreparedQuarantine> {
    greenlit_engine::validate_v0_support(&workflow).map_err(|error| errors::plan_error(&error))?;
    let authored_extraction = greenlit_workflow::extract_static(&workflow)
        .map_err(|error| errors::parse_error(&error))?;
    let (planning_vars, unresolved) =
        planning_variables(&authored_extraction, args, local_variables)?;

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
        unresolved.as_deref(),
    );
    let extraction = greenlit_workflow::extract_static(&reachable)
        .map_err(|error| errors::parse_error(&error))?;
    let resolved_vars = match vars::resolve_vars(
        &extraction,
        &args.vars,
        local_variables.entries(),
        local_variables.exists(),
    ) {
        Ok(value) => value,
        Err(error) if remote_could_help(&error) => planning_vars,
        Err(error) => return Err(local_var_error(error)),
    };

    let additional = source_findings(&extraction, args, local_variables)?;
    let empty_secrets = Value::object(Vec::<(String, Value)>::new());
    let inputs = greenlit_runtime::RuntimeCapabilityInputs::new(
        &event.github,
        &empty_secrets,
        None,
        false,
        args.write_back,
    );
    // The runtime assessment walks the selected plan's typed `DeferReason`
    // inventory, so computed indexes, wildcards, and whole-context
    // `secrets`/`github` references cannot disappear through this source-only
    // extraction layer. `additional` retains facts that are not represented
    // in the execution plan, such as unresolved remote variables.
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
    let decision = assessment.decision();
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

pub(crate) fn render_degraded(assessment: &greenlit_runtime::RuntimeCapabilityAssessment) {
    let decision = assessment.decision();
    if decision.outcome() != QuarantineOutcome::Degraded {
        return;
    }
    eprintln!(
        "warning: `--allow-degraded` forced {} uncertified capability finding(s); compatibility is degraded and assurance is none",
        decision.forced_findings().len()
    );
    for resolved in decision.forced_findings() {
        let finding = resolved.finding();
        eprintln!(
            "  forced: {} at {} — {}",
            finding.capability_id(),
            finding.scope(),
            finding.reason()
        );
    }
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

fn planning_variables(
    extraction: &greenlit_workflow::StaticExtraction,
    args: &RunArgs,
    local_variables: &LocalVariables,
) -> anyhow::Result<(Value, Option<Box<vars::VarResolutionError>>)> {
    match vars::resolve_vars(
        extraction,
        &args.vars,
        local_variables.entries(),
        local_variables.exists(),
    ) {
        Ok(value) => Ok((value, None)),
        Err(error) if remote_could_help(&error) => {
            let mut provisional = args.vars.clone();
            provisional.extend(
                error
                    .missing
                    .iter()
                    .map(|missing| (missing.name.clone(), String::new())),
            );
            if error.dynamic_lookup.is_some() && provisional.is_empty() {
                provisional.push(("GREENLIT_PHASE12_DYNAMIC_MAP".to_string(), String::new()));
            }
            let values = vars::resolve_vars(
                extraction,
                &provisional,
                local_variables.entries(),
                local_variables.exists(),
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "could not construct a conservative local variable context\n  fix: preserve the workflow and file a Greenlit defect"
                )
            })?;
            Ok((values, Some(error)))
        }
        Err(error) => Err(local_var_error(error)),
    }
}

fn remote_could_help(error: &vars::VarResolutionError) -> bool {
    error.invalid.is_empty()
        && error.oversized.is_empty()
        && error.ambiguous_process.is_empty()
        && error.non_unicode_process.is_empty()
        && (!error.missing.is_empty() || error.dynamic_lookup.is_some())
}

fn source_findings(
    extraction: &greenlit_workflow::StaticExtraction,
    args: &RunArgs,
    local_variables: &LocalVariables,
) -> anyhow::Result<Vec<CapabilityFinding>> {
    let mut findings = Vec::new();
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
    if let Err(error) = vars::resolve_vars(
        extraction,
        &args.vars,
        local_variables.entries(),
        local_variables.exists(),
    ) {
        if !remote_could_help(&error) {
            return Err(local_var_error(error));
        }
        for missing in &error.missing {
            findings.push(CapabilityFinding::new(
                CAPABILITY_VARIABLE_REMOTE,
                missing.first_use.to_string(),
                format!(
                    "`vars.{}` requires repository or organization lookup",
                    missing.name
                ),
            ));
        }
        if let Some(span) = &error.dynamic_lookup {
            findings.push(CapabilityFinding::new(
                CAPABILITY_VARIABLE_REMOTE,
                span.to_string(),
                "a dynamic variable lookup requires the complete remote variable map",
            ));
            findings.push(CapabilityFinding::new(
                CAPABILITY_REACHABILITY_AMBIGUOUS,
                span.to_string(),
                "a dynamic variable lookup prevents exact pre-engine reachability proof",
            ));
        }
    }
    if args.write_back {
        findings.push(CapabilityFinding::new(
            CAPABILITY_SOURCE_WRITE_BACK,
            "run.write-back",
            "write-back would apply untrusted sandbox changes to the host worktree",
        ));
    }
    Ok(findings)
}

fn references_github_token(extraction: &greenlit_workflow::StaticExtraction) -> bool {
    extraction.references_github_token
        || extraction.secrets.contains_key(secrets::GITHUB_TOKEN_NAME)
}

fn local_var_error(error: Box<vars::VarResolutionError>) -> anyhow::Error {
    match errors::vars_outcome(vars::VarsOutcome::LocalError(error)) {
        Err(error) => error,
        Ok(_) => anyhow::anyhow!(
            "local variable validation returned an inconsistent outcome\n  fix: preserve the workflow and file a Greenlit defect"
        ),
    }
}
