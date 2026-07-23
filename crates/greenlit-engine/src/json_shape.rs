//! The stable JSON value-object shape shared by conditions, outputs, and
//! other planned fields, implementing `PHASE-1-engine-core.md`'s stable
//! `litci plan --json` contract.
//!
//! `evaluation` is a closed two-value enum (`"static" | "deferred"`);
//! `source` preserves expression/template text verbatim, while typed YAML
//! literals use their canonical value spelling because the workflow model
//! intentionally does not retain scalar lexemes. `value` appears only for
//! `"static"`; `residual`/`defers_on` appear only for `"deferred"`.
//! `defers_on[].kind` is deliberately open (new variants are additive) —
//! this is exactly what [`DeferReason`]'s manual [`serde::Serialize`] impl
//! below produces.

use serde::{Serialize, Serializer};

use crate::defer::{DeferReason, StatusFn, StepStatusField};

/// Serializes a `greenlit_workflow::Span` as its `Display` rendering
/// (`file:line:col`, per [`greenlit_workflow::Span`]'s own `Display` impl)
/// — that crate carries no `serde` dependency, so this crate cannot derive
/// or implement `Serialize` on the foreign type directly (the orphan
/// rule); a plain location string is a stable, sufficient JSON
/// representation for a plan-time lint's location.
pub(crate) fn serialize_span<S>(
    span: &greenlit_workflow::Span,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&span.to_string())
}

/// The shared tagged-union shape for every serializable planned value.
#[derive(Debug, Serialize)]
pub(crate) struct EvaluatedJson<'a, T: Serialize> {
    /// Authored field location.
    pub span: String,
    /// Verbatim expression/template text, or a typed YAML literal's
    /// canonical spelling.
    pub source: &'a str,
    /// `"static"` or `"deferred"`.
    pub evaluation: &'static str,
    /// Present only when `evaluation == "static"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<&'a T>,
    /// Present only when `evaluation == "deferred"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub residual: Option<&'a str>,
    /// Present only when `evaluation == "deferred"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defers_on: Option<&'a [DeferReason]>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum DeferReasonJson<'a> {
    NeedsOutput {
        job: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<&'a str>,
    },
    NeedsResult {
        job: &'a str,
    },
    StepOutput {
        step: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<&'a str>,
    },
    StepStatus {
        step: &'a str,
        field: &'static str,
    },
    StatusFunction {
        function: &'static str,
    },
    HashFiles,
    DynamicEnv {
        name: &'a str,
    },
    GithubContext {
        #[serde(skip_serializing_if = "Option::is_none")]
        property: Option<&'a str>,
    },
    RunnerContext,
    JobContext,
    MatrixContext,
    StrategyContext,
    SecretsContext,
    NeedsContext,
    StepsContext,
}

impl Serialize for DeferReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let json = match self {
            DeferReason::NeedsOutput { job, output } => DeferReasonJson::NeedsOutput {
                job: &job.0,
                output: output.as_deref(),
            },
            DeferReason::NeedsResult { job } => DeferReasonJson::NeedsResult { job: &job.0 },
            DeferReason::StepOutput { step, output } => DeferReasonJson::StepOutput {
                step,
                output: output.as_deref(),
            },
            DeferReason::StepStatus { step, field } => DeferReasonJson::StepStatus {
                step,
                field: match field {
                    StepStatusField::Outcome => "outcome",
                    StepStatusField::Conclusion => "conclusion",
                },
            },
            DeferReason::StatusFn(f) => DeferReasonJson::StatusFunction {
                function: match f {
                    StatusFn::Success => "success",
                    StatusFn::Failure => "failure",
                    StatusFn::Cancelled => "cancelled",
                    StatusFn::Always => "always",
                },
            },
            DeferReason::HashFiles => DeferReasonJson::HashFiles,
            DeferReason::DynamicEnv { name } => DeferReasonJson::DynamicEnv { name },
            DeferReason::GithubContext { property } => DeferReasonJson::GithubContext {
                property: property.as_deref(),
            },
            DeferReason::RunnerContext => DeferReasonJson::RunnerContext,
            DeferReason::JobContext => DeferReasonJson::JobContext,
            DeferReason::MatrixContext => DeferReasonJson::MatrixContext,
            DeferReason::StrategyContext => DeferReasonJson::StrategyContext,
            DeferReason::SecretsContext => DeferReasonJson::SecretsContext,
            DeferReason::NeedsContextWhole => DeferReasonJson::NeedsContext,
            DeferReason::StepsContextWhole => DeferReasonJson::StepsContext,
        };
        json.serialize(serializer)
    }
}
