//! Planned mirrors of environment and container configuration.
//!
//! These fields are context-sensitive in GitHub Actions. Phase 1 folds
//! everything available locally and preserves an explicit residual for the
//! execution phase instead of carrying ambiguous raw strings forward.

use indexmap::IndexMap;
use serde::{Serialize, Serializer};

use greenlit_workflow::model::job::ContainerSpec;
use greenlit_workflow::model::value::ScalarOrExpr;

use crate::defer::DeferReason;
use crate::partial_eval::{FoldCtx, LocatedEvalError};
use crate::plan::{PlanSizeBudget, RetainedFieldError};
use crate::planned::{Evaluation, Planned, plan_scalar_string};

/// A planned string-valued `env:`/`with:`/container field.
pub type EnvValue = Planned<String>;

/// Plans one `env:` or `with:` layer in declaration order.
pub(crate) fn plan_env_layer(
    entries: &[(
        greenlit_workflow::Spanned<String>,
        greenlit_workflow::Spanned<ScalarOrExpr>,
    )],
    ctx: &FoldCtx<'_>,
    size_budget: &mut PlanSizeBudget,
) -> Result<IndexMap<String, EnvValue>, RetainedFieldError> {
    let mut planned = IndexMap::with_capacity(entries.len());
    for (name, value) in entries {
        let planned_value = plan_scalar_string(value, ctx).map_err(|source| LocatedEvalError {
            span: value.span.clone(),
            source,
        })?;
        // Charge the key and value before the map retains them. This keeps a
        // large collection of individually bounded templates from crossing
        // the aggregate plan cap between checks.
        size_budget.add(&(name.value.as_str(), &planned_value), &value.span)?;
        planned.insert(name.value.clone(), planned_value);
    }
    Ok(planned)
}

/// `container:` or one `services.<id>:` entry, resolved as far as plan time
/// permits.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContainerPlan {
    /// `image:`.
    pub image: EnvValue,
    /// `credentials:`, if present.
    pub credentials: Option<ContainerCredentials>,
    /// `env:`.
    pub env: IndexMap<String, EnvValue>,
    /// `ports:`.
    pub ports: Vec<EnvValue>,
    /// `volumes:`.
    pub volumes: Vec<EnvValue>,
    /// `options:`, if present.
    pub options: Option<EnvValue>,
}

/// `container.credentials:`, resolved as far as plan time permits.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContainerCredentials {
    /// `credentials.username:`, if present.
    pub username: Option<EnvValue>,
    /// `credentials.password:`, if present. The in-memory value remains
    /// available to execution, while stable plan JSON redacts the authored
    /// source, resolved value, and residual expression.
    #[serde(serialize_with = "serialize_redacted_password")]
    pub password: Option<EnvValue>,
}

const MASKED_PASSWORD: &str = "[masked]";

#[derive(Serialize)]
struct RedactedPasswordJson<'a> {
    span: String,
    source: &'static str,
    evaluation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    residual: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    defers_on: Option<&'a [DeferReason]>,
}

struct RedactedPassword<'a>(&'a EnvValue);

impl Serialize for RedactedPassword<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (evaluation, value, residual, defers_on) = match &self.0.evaluation {
            Evaluation::Static(_) => ("static", Some(MASKED_PASSWORD), None, None),
            Evaluation::Deferred(deferred) => (
                "deferred",
                None,
                Some(MASKED_PASSWORD),
                Some(deferred.defers_on.as_slice()),
            ),
        };
        RedactedPasswordJson {
            span: self.0.span.to_string(),
            source: MASKED_PASSWORD,
            evaluation,
            value,
            residual,
            defers_on,
        }
        .serialize(serializer)
    }
}

fn serialize_redacted_password<S>(
    password: &Option<EnvValue>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    password
        .as_ref()
        .map(RedactedPassword)
        .serialize(serializer)
}

pub(crate) fn plan_container(
    spec: &ContainerSpec,
    ctx: &FoldCtx<'_>,
    size_budget: &mut PlanSizeBudget,
) -> Result<ContainerPlan, RetainedFieldError> {
    let image = plan_string_field(&spec.image, ctx)?;
    size_budget.add(&image, &spec.image.span)?;
    let credentials = match &spec.credentials {
        None => None,
        Some(credentials) => {
            let username = credentials
                .value
                .username
                .as_ref()
                .map(|value| {
                    let planned = plan_string_field(value, ctx)?;
                    size_budget.add(&planned, &value.span)?;
                    Ok::<_, RetainedFieldError>(planned)
                })
                .transpose()?;
            let password = credentials
                .value
                .password
                .as_ref()
                .map(|value| {
                    let planned = plan_string_field(value, ctx)?;
                    // Count the retained secret-bearing value, not merely
                    // its compact redacted JSON view.
                    size_budget.add(&planned, &value.span)?;
                    Ok::<_, RetainedFieldError>(planned)
                })
                .transpose()?;
            Some(ContainerCredentials { username, password })
        }
    };
    let env = plan_env_layer(&spec.env, ctx, size_budget)?;
    let mut ports = Vec::with_capacity(spec.ports.len());
    for value in &spec.ports {
        let planned = plan_string_field(value, ctx)?;
        size_budget.add(&planned, &value.span)?;
        ports.push(planned);
    }
    let mut volumes = Vec::with_capacity(spec.volumes.len());
    for value in &spec.volumes {
        let planned = plan_string_field(value, ctx)?;
        size_budget.add(&planned, &value.span)?;
        volumes.push(planned);
    }
    let options = spec
        .options
        .as_ref()
        .map(|value| {
            let planned = plan_string_field(value, ctx)?;
            size_budget.add(&planned, &value.span)?;
            Ok::<_, RetainedFieldError>(planned)
        })
        .transpose()?;

    size_budget.add_bytes(128, &spec.image.span)?;

    Ok(ContainerPlan {
        image,
        credentials,
        env,
        ports,
        volumes,
        options,
    })
}

fn plan_string_field(
    value: &greenlit_workflow::Spanned<ScalarOrExpr>,
    ctx: &FoldCtx<'_>,
) -> Result<EnvValue, LocatedEvalError> {
    plan_scalar_string(value, ctx).map_err(|source| LocatedEvalError {
        span: value.span.clone(),
        source,
    })
}
