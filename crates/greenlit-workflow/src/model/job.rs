//! `jobs.<id>:` — everything under a single job.

use crate::model::step::Step;
use crate::model::value::{ScalarOrExpr, UnsupportedConstruct, YamlValue};
use crate::model::workflow::Defaults;
use crate::span::{Span, Spanned};

/// One entry of `jobs:`, in file order.
///
/// Covers `runs-on`, `needs`, `if`, `outputs`, `env`, `defaults`,
/// `strategy.matrix` (`include`/`exclude`/`fail-fast`/`max-parallel`),
/// `services`, `container`, and `steps`, per `PHASE-1-engine-core.md`.
/// `name` is additionally modeled (see crate docs' "Known deviations" —
/// omitting a basic display-name string would make nearly every
/// realistically-named job fail to parse). Job-level `permissions:` is not
/// modeled (same docs section). A job defined as a reusable-workflow call
/// (`jobs.<id>.uses:` instead of `steps:`) is recognized via
/// [`Job::reusable_call`] rather than deeply parsed — reusable workflows
/// are out of v0 scope.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// The complete `jobs.<id>` configuration mapping.
    pub span: Span,
    /// The job id (the `jobs:` mapping key).
    pub id: Spanned<String>,
    /// `name:` — display name.
    pub name: Option<Spanned<String>>,
    /// Absent only when [`Job::reusable_call`] is `Some` (reusable-workflow
    /// call jobs have no `runs-on`).
    pub runs_on: Option<Spanned<RunsOn>>,
    /// `needs:`, normalized from either a single string or a list.
    pub needs: Vec<Spanned<String>>,
    /// `if:` — raw expression text, with or without the `${{ }}` wrapper.
    pub if_condition: Option<Spanned<String>>,
    /// `outputs:` — raw expression text per declared output name, retained
    /// (not evaluated) for `greenlit-engine`'s Phase 2 finalization per
    /// `PHASE-1-engine-core.md`'s greenlit-engine section.
    pub outputs: Vec<(Spanned<String>, Spanned<String>)>,
    /// `env:` at the job level.
    pub env: Vec<(Spanned<String>, Spanned<ScalarOrExpr>)>,
    /// `defaults:` at the job level.
    pub defaults: Option<Spanned<Defaults>>,
    /// `strategy:`.
    pub strategy: Option<Spanned<Strategy>>,
    /// `services:`, in file order.
    pub services: Vec<(Spanned<String>, Spanned<ContainerSpec>)>,
    /// `container:`.
    pub container: Option<Spanned<ContainerSpec>>,
    /// `steps:`, in file order. Empty only when [`Job::reusable_call`] is
    /// `Some`.
    pub steps: Vec<Step>,
    /// `environment:` — recognized but rejected (deployments/environments
    /// are out of v0 scope).
    pub environment: Option<UnsupportedConstruct>,
    /// `concurrency:` at the job level — same construct as the
    /// workflow-level one, recognized but rejected.
    pub concurrency: Option<UnsupportedConstruct>,
    /// `uses:` at the job level (a reusable-workflow call in place of
    /// `steps:`) — recognized but rejected.
    pub reusable_call: Option<UnsupportedConstruct>,
}

/// `runs-on:`.
#[derive(Debug, Clone, PartialEq)]
pub enum RunsOn {
    /// A single label, e.g. `runs-on: ubuntu-latest` — this is also the
    /// form used for `runs-on: ${{ matrix.os }}`, a very common pattern;
    /// this crate preserves the raw text without evaluating it (validating
    /// it against the supported label set is `greenlit-engine`'s job).
    Label(Spanned<String>),
    /// `runs-on: [self-hosted, linux, x64]` — AND-matched labels.
    Labels(Vec<Spanned<String>>),
    /// `runs-on: { group: …, labels: […] }`.
    Group {
        /// The `group:` name, if given.
        group: Option<Spanned<String>>,
        /// The `labels:` list.
        labels: Vec<Spanned<String>>,
    },
}

/// `strategy:`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Strategy {
    /// `strategy.matrix:`.
    pub matrix: Option<Spanned<MatrixSource>>,
    /// `strategy.fail-fast:`.
    pub fail_fast: Option<Spanned<ScalarOrExpr>>,
    /// `strategy.max-parallel:`.
    pub max_parallel: Option<Spanned<ScalarOrExpr>>,
}

/// `strategy.matrix:` — either an inline matrix definition, or a single
/// whole-value expression (the documented `strategy.matrix: ${{ fromJSON(...) }}`
/// pattern — design memo §5.2 "matrix" row).
#[derive(Debug, Clone, PartialEq)]
pub enum MatrixSource {
    /// A literal `strategy.matrix:` mapping.
    Inline(Matrix),
    /// A whole-value expression, e.g. `${{ fromJSON(needs.setup.outputs.matrix) }}`.
    Expression(Spanned<String>),
}

/// One matrix entry: an ordered set of `axis: value` pairs, used for both
/// `include` and `exclude` lists.
pub type MatrixEntry = Vec<(Spanned<String>, Spanned<YamlValue>)>;

/// An inline `strategy.matrix:` mapping.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Matrix {
    /// Every top-level key except `include`/`exclude`, each with its axis
    /// source. A literal axis is represented by all sequence items. An
    /// expression-backed axis is represented by exactly one
    /// [`YamlValue::Scalar`] containing [`ScalarOrExpr::Expression`]; the
    /// engine must expand the expression's resulting array rather than
    /// treating that array as one matrix value.
    pub axes: Vec<(Spanned<String>, Vec<Spanned<YamlValue>>)>,
    /// `matrix.include:`.
    pub include: Vec<Spanned<MatrixEntry>>,
    /// `matrix.exclude:`.
    pub exclude: Vec<Spanned<MatrixEntry>>,
}

/// `container:` or one `services.<id>:` entry — identical shape
/// (`image, credentials, env, ports, volumes, options`) per
/// `PHASE-1-engine-core.md`.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSpec {
    /// `image:` — required; may be the bare-string shorthand form of the
    /// whole `container:`/`services.<id>:` value.
    pub image: Spanned<ScalarOrExpr>,
    /// `credentials:`.
    pub credentials: Option<Spanned<Credentials>>,
    /// `env:`.
    pub env: Vec<(Spanned<String>, Spanned<ScalarOrExpr>)>,
    /// `ports:`.
    pub ports: Vec<Spanned<ScalarOrExpr>>,
    /// `volumes:`.
    pub volumes: Vec<Spanned<ScalarOrExpr>>,
    /// `options:`.
    pub options: Option<Spanned<ScalarOrExpr>>,
}

/// `container.credentials:`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Credentials {
    /// `credentials.username:`.
    pub username: Option<Spanned<ScalarOrExpr>>,
    /// `credentials.password:`.
    pub password: Option<Spanned<ScalarOrExpr>>,
}
