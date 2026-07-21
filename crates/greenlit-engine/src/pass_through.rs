//! Small serializable mirrors of `greenlit-workflow` value shapes that this
//! phase carries through the plan *unevaluated* — `env:`/`with:`
//! interpolation is explicitly a later call site for the partial evaluator
//! (design memo §5: "job `if`, step `if`, output values, and (later)
//! `env:`/`with:` interpolations"), so Phase 1 only needs to preserve these
//! faithfully (declaration order, literal-vs-expression distinction), not
//! resolve them.

use indexmap::IndexMap;
use serde::Serialize;

use greenlit_workflow::model::job::ContainerSpec;
use greenlit_workflow::model::value::{ScalarOrExpr, YamlScalar};

/// A GitHub YAML scalar literal, mirrored with [`Serialize`] (the workflow
/// crate's own [`YamlScalar`] intentionally carries no `serde` dependency).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LiteralValue {
    /// `null`.
    Null,
    /// `true`/`false`.
    Bool(bool),
    /// A number.
    Number(f64),
    /// A string.
    String(String),
}

impl From<&YamlScalar> for LiteralValue {
    fn from(s: &YamlScalar) -> Self {
        match s {
            YamlScalar::Null => LiteralValue::Null,
            YamlScalar::Bool(b) => LiteralValue::Bool(*b),
            YamlScalar::Number(n) => LiteralValue::Number(*n),
            YamlScalar::String(s) => LiteralValue::String(s.clone()),
        }
    }
}

/// A pass-through `env:`/`with:`/container-field value: either a literal,
/// or unevaluated `${{ }}` source text.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum EnvValue {
    /// Resolved to a literal at parse time (no `${{ }}` anywhere).
    Literal {
        /// The literal value.
        value: LiteralValue,
    },
    /// Raw, unevaluated source text containing at least one `${{ }}`
    /// placeholder — resolving this is a later phase's job.
    Expression {
        /// The verbatim source text.
        source: String,
    },
}

impl From<&ScalarOrExpr> for EnvValue {
    fn from(v: &ScalarOrExpr) -> Self {
        match v {
            ScalarOrExpr::Literal(s) => EnvValue::Literal { value: s.into() },
            ScalarOrExpr::Expression(raw) => EnvValue::Expression {
                source: raw.clone(),
            },
        }
    }
}

/// A pass-through mirror of one `env:` layer (workflow-, job-, or
/// step-level), preserving declaration order.
pub(crate) fn env_layer_to_map(
    entries: &[(
        greenlit_workflow::Spanned<String>,
        greenlit_workflow::Spanned<ScalarOrExpr>,
    )],
) -> IndexMap<String, EnvValue> {
    entries
        .iter()
        .map(|(k, v)| (k.value.clone(), EnvValue::from(&v.value)))
        .collect()
}

/// `container:`/one `services.<id>:` entry, pass-through.
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

/// `container.credentials:`, pass-through.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContainerCredentials {
    /// `credentials.username:`, if present.
    pub username: Option<EnvValue>,
    /// `credentials.password:`, if present.
    pub password: Option<EnvValue>,
}

pub(crate) fn container_plan_from(spec: &ContainerSpec) -> ContainerPlan {
    ContainerPlan {
        image: EnvValue::from(&spec.image.value),
        credentials: spec.credentials.as_ref().map(|c| ContainerCredentials {
            username: c.value.username.as_ref().map(|u| EnvValue::from(&u.value)),
            password: c.value.password.as_ref().map(|p| EnvValue::from(&p.value)),
        }),
        env: env_layer_to_map(&spec.env),
        ports: spec
            .ports
            .iter()
            .map(|p| EnvValue::from(&p.value))
            .collect(),
        volumes: spec
            .volumes
            .iter()
            .map(|v| EnvValue::from(&v.value))
            .collect(),
        options: spec.options.as_ref().map(|o| EnvValue::from(&o.value)),
    }
}
