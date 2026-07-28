use greenlit_engine::{
    CAPABILITY_CREDENTIAL_GITHUB, CAPABILITY_DISPATCH_INPUT, CAPABILITY_EVIDENCE_INTEGRITY,
    CAPABILITY_INFRASTRUCTURE_DIND, CAPABILITY_SECRET_CONTEXT, CAPABILITY_SECURITY_BOUNDARY,
    CAPABILITY_SOURCE_WRITE_BACK, CAPABILITY_VARIABLE_CONTEXT, CapabilityFinding, ExecutionPlan,
    QuarantineDecision, decide_capability_quarantine,
};
use greenlit_expr::Value;

use super::super::{ExecError, RunConfig};
use super::{collect_job_findings, plan_contexts};

/// Explicit authorization applied by the authoritative runtime quarantine.
///
/// There is deliberately no default and no bypass variant. A caller that
/// executes an uncertified shell step must name [`Self::AllowDegradedShell`];
/// protected findings remain blocked regardless of the selected variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAuthorization {
    /// Enforce certification without accepting degraded execution.
    Enforce,
    /// Accept only registry-forceable shell degradation without assurance.
    AllowDegradedShell,
}

impl RuntimeAuthorization {
    const fn allows_degraded(self) -> bool {
        matches!(self, Self::AllowDegradedShell)
    }
}

/// Explicit runtime authorization paired with cooperative cancellation.
///
/// Pairing these controls keeps the event-capable entrypoint within the
/// public API's argument budget while preserving an unavoidable authorization
/// choice. There is no default constructor.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeControl<'a> {
    pub(in crate::executor) authorization: RuntimeAuthorization,
    pub(in crate::executor) cancellation: &'a crate::Cancellation,
    pub(in crate::executor) assessment: Option<&'a RuntimeCapabilityAssessment>,
}

impl<'a> RuntimeControl<'a> {
    /// Creates runtime control with an explicit quarantine authorization.
    #[must_use]
    pub const fn new(
        authorization: RuntimeAuthorization,
        cancellation: &'a crate::Cancellation,
    ) -> Self {
        Self {
            authorization,
            cancellation,
            assessment: None,
        }
    }

    /// Creates runtime control bound to the exact capability decision already
    /// rendered and retained by the caller.
    ///
    /// The executor re-derives only the plan/configuration finding inventory
    /// and rejects a mismatch. It does not silently replace this assessment
    /// with a second decision.
    #[must_use]
    pub const fn with_assessment(
        authorization: RuntimeAuthorization,
        cancellation: &'a crate::Cancellation,
        assessment: &'a RuntimeCapabilityAssessment,
    ) -> Self {
        Self {
            authorization,
            cancellation,
            assessment: Some(assessment),
        }
    }
}

/// Sensitive runtime inputs that participate in capability assessment.
///
/// This deliberately excludes stores, image locks, and engine handles so the
/// CLI can assess a selected plan before constructing any component that can
/// retrieve credentials, contact a network, or access the container engine.
pub struct RuntimeCapabilityInputs<'a> {
    github: &'a Value,
    vars: &'a Value,
    inputs: &'a Value,
    secrets: &'a Value,
    action_github_token: Option<&'a str>,
    dind: bool,
    write_back: bool,
}

impl<'a> RuntimeCapabilityInputs<'a> {
    /// Creates the exact sensitive-input shape the eventual [`RunConfig`]
    /// must retain.
    #[must_use]
    pub const fn new(
        github: &'a Value,
        vars: &'a Value,
        inputs: &'a Value,
        secrets: &'a Value,
        action_github_token: Option<&'a str>,
        dind: bool,
        write_back: bool,
    ) -> Self {
        Self {
            github,
            vars,
            inputs,
            secrets,
            action_github_token,
            dind,
            write_back,
        }
    }

    fn from_config(config: &'a RunConfig) -> Self {
        Self {
            github: &config.github,
            vars: &config.vars,
            inputs: &config.inputs,
            secrets: &config.secrets,
            action_github_token: config.actions.github_token.as_deref(),
            dind: config.dind,
            write_back: config.write_back,
        }
    }
}

/// Opaque, exact quarantine assessment for one selected plan and sensitive
/// runtime-input shape.
#[derive(Debug, Clone)]
pub struct RuntimeCapabilityAssessment {
    runtime_findings: Vec<CapabilityFinding>,
    decision: QuarantineDecision,
    allow_degraded: bool,
}

impl RuntimeCapabilityAssessment {
    /// Exact registry decision used for diagnostics and retained evidence.
    #[must_use]
    pub const fn decision(&self) -> &QuarantineDecision {
        &self.decision
    }
}

/// Assesses a selected plan before any credential, action, network, or engine
/// preparation.
///
/// `additional_findings` carries source-located workflow facts whose values
/// are intentionally absent from [`ExecutionPlan`], such as reachable
/// `secrets.*`, `github.token`, unresolved remote variables, and write-back.
/// Their order and duplicates are retained exactly.
#[must_use]
pub fn assess_runtime_capabilities(
    plan: &ExecutionPlan,
    inputs: &RuntimeCapabilityInputs<'_>,
    additional_findings: &[CapabilityFinding],
    allow_degraded: bool,
) -> RuntimeCapabilityAssessment {
    let runtime_findings = runtime_capability_findings(plan, inputs);
    let mut findings = Vec::with_capacity(additional_findings.len() + runtime_findings.len());
    findings.extend_from_slice(additional_findings);
    findings.extend(runtime_findings.iter().cloned());
    RuntimeCapabilityAssessment {
        runtime_findings,
        decision: decide_capability_quarantine(&findings, allow_degraded),
        allow_degraded,
    }
}

pub(in crate::executor) fn enforce_runtime_quarantine(
    plan: &ExecutionPlan,
    config: &RunConfig,
    authorization: RuntimeAuthorization,
    assessment: Option<&RuntimeCapabilityAssessment>,
) -> Result<(), ExecError> {
    let inputs = RuntimeCapabilityInputs::from_config(config);
    let current_findings = runtime_capability_findings(plan, &inputs);
    let fallback;
    let decision = if let Some(assessment) = assessment {
        if assessment.allow_degraded != authorization.allows_degraded()
            || assessment.runtime_findings != current_findings
        {
            return Err(ExecError::CapabilityQuarantined {
                capability_id: CAPABILITY_EVIDENCE_INTEGRITY.to_string(),
                scope: "runtime.capability-assessment".to_string(),
                reason: "the execution plan or sensitive runtime inputs differ from the pre-preparation capability assessment".to_string(),
                fix: "preserve the run directory and file a Greenlit defect".to_string(),
            });
        }
        &assessment.decision
    } else {
        fallback = decide_capability_quarantine(&current_findings, authorization.allows_degraded());
        &fallback
    };
    blocking_error(decision).map_or(Ok(()), Err)
}

fn blocking_error(decision: &QuarantineDecision) -> Option<ExecError> {
    let blocked = decision.blocking_findings().first()?;
    let finding = blocked.finding();
    Some(ExecError::CapabilityQuarantined {
        capability_id: finding.capability_id().to_string(),
        scope: finding.scope().to_string(),
        reason: finding.reason().to_string(),
        fix: blocked.user_action().to_string(),
    })
}

fn collect_runtime_context_findings(
    vars: &Value,
    inputs: &Value,
    findings: &mut Vec<CapabilityFinding>,
) {
    match inputs {
        Value::Object(inputs) if inputs.is_empty() => {}
        Value::Object(_) => findings.push(CapabilityFinding::new(
            CAPABILITY_DISPATCH_INPUT,
            "run-config.inputs",
            "the runtime received a non-empty input context before credential-bearing input preflight was certified",
        )),
        _ => findings.push(CapabilityFinding::new(
            CAPABILITY_SECURITY_BOUNDARY,
            "run-config.inputs",
            "the runtime cannot prove that a non-object input context is empty",
        )),
    }
    match vars {
        Value::Object(vars) if vars.is_empty() => {}
        Value::Object(_) => findings.push(CapabilityFinding::new(
            CAPABILITY_VARIABLE_CONTEXT,
            "run-config.vars",
            "the runtime received a non-empty variable context before trust and input preflight was certified",
        )),
        _ => findings.push(CapabilityFinding::new(
            CAPABILITY_SECURITY_BOUNDARY,
            "run-config.vars",
            "the runtime cannot prove that a non-object variable context is empty",
        )),
    }
}

fn runtime_capability_findings(
    plan: &ExecutionPlan,
    inputs: &RuntimeCapabilityInputs<'_>,
) -> Vec<CapabilityFinding> {
    let mut findings = Vec::new();
    collect_runtime_context_findings(inputs.vars, inputs.inputs, &mut findings);
    match inputs.secrets {
        Value::Object(secrets) if secrets.is_empty() => {}
        Value::Object(_) => findings.push(CapabilityFinding::new(
            CAPABILITY_SECRET_CONTEXT,
            "run-config.secrets",
            "the runtime received a non-empty secret context",
        )),
        _ => findings.push(CapabilityFinding::new(
            CAPABILITY_SECURITY_BOUNDARY,
            "run-config.secrets",
            "the runtime cannot prove that a non-object secret context is empty",
        )),
    }
    collect_github_context_token(inputs.github, &mut findings);
    match inputs.action_github_token {
        None => {}
        Some("") => findings.push(CapabilityFinding::new(
            CAPABILITY_SECURITY_BOUNDARY,
            "run-config.actions.github-token",
            "the action runtime received a malformed empty GitHub credential",
        )),
        Some(_) => findings.push(CapabilityFinding::new(
            CAPABILITY_CREDENTIAL_GITHUB,
            "run-config.actions.github-token",
            "the action runtime received a GitHub credential",
        )),
    }
    if inputs.dind {
        findings.push(CapabilityFinding::new(
            CAPABILITY_INFRASTRUCTURE_DIND,
            "run-config.dind",
            "the runtime was asked to start a privileged Docker-in-Docker sidecar",
        ));
    }
    if inputs.write_back {
        findings.push(CapabilityFinding::new(
            CAPABILITY_SOURCE_WRITE_BACK,
            "run-config.write-back",
            "the runtime was asked to retain sandbox state for host write-back",
        ));
    }

    plan_contexts::collect(plan, &mut findings);
    for job in &plan.jobs {
        collect_job_findings(job, &mut findings);
    }
    findings
}

fn collect_github_context_token(github: &Value, findings: &mut Vec<CapabilityFinding>) {
    let Value::Object(github) = github else {
        findings.push(CapabilityFinding::new(
            CAPABILITY_SECURITY_BOUNDARY,
            "run-config.github",
            "the runtime cannot prove that a non-object GitHub context is credential-free",
        ));
        return;
    };
    match github.get("token") {
        None | Some(Value::Null) => {}
        Some(Value::String(token)) if token.is_empty() => {}
        Some(Value::String(_)) => findings.push(CapabilityFinding::new(
            CAPABILITY_CREDENTIAL_GITHUB,
            "run-config.github.token",
            "the workflow-visible GitHub context contains a bearer token",
        )),
        Some(_) => findings.push(CapabilityFinding::new(
            CAPABILITY_SECURITY_BOUNDARY,
            "run-config.github.token",
            "the workflow-visible GitHub token has an ambiguous non-string shape",
        )),
    }
}
