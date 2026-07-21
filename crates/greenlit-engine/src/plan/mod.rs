//! [`ExecutionPlan`]: the fully resolved plan `litci plan` prints, and
//! [`plan()`], the crate's top-level entrypoint.
//!
//! Assembles everything the other modules compute: the `needs` graph
//! (`crate::graph`), matrix expansion (`crate::matrix`), supported-runner
//! resolution (`crate::runner`), and partially-evaluated conditions/outputs
//! (`crate::condition`/`crate::outputs`) into one ordered, serializable
//! document. See `PHASE-1-engine-core.md`'s greenlit-engine section for the
//! exact field list this type must carry. Split across submodules by
//! concern: `job` (assembling one [`JobPlan`]), `step` (one [`StepPlan`]),
//! and `skip` (the static-skip propagation pass).

mod job;
mod skip;
mod step;

use indexmap::IndexMap;
use serde::Serialize;

use greenlit_expr::Value;
use greenlit_workflow::Span;
use greenlit_workflow::model::job::Job;
use greenlit_workflow::model::workflow::Workflow;

use crate::condition::Condition;
use crate::event::SyntheticEvent;
use crate::graph::{GraphError, JobId, build_graph};
use crate::lints::Lint;
use crate::matrix::{DEFAULT_MAX_MATRIX_LEGS, MatrixError, StrategyPlan};
use crate::outputs::JobOutputsPlan;
use crate::partial_eval::PartialEvalError;
use crate::pass_through::{ContainerPlan, EnvValue};
use crate::runner::{RunnerError, RunnerImage};

/// The fully resolved plan for one workflow file plus one synthetic event.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionPlan {
    /// The simulated trigger's event name (`"push"`, `"pull_request"`,
    /// `"workflow_dispatch"`).
    pub event_name: String,
    /// Workflow-level `env:` entries. Runtime layering is represented
    /// explicitly as this outer layer, [`JobPlan::env`], then
    /// [`StepPlan::env`], with the more specific layer winning.
    pub env: IndexMap<String, EnvValue>,
    /// Every job, in the workflow file's declaration order.
    pub jobs: Vec<JobPlan>,
    /// The deterministic topological order (design memo §2.3) — the order
    /// a scheduler would start jobs in, respecting `needs`.
    pub topo_order: Vec<JobId>,
    /// Every plan-time warning raised while building this plan.
    pub lints: Vec<Lint>,
}

/// One job, fully resolved as far as plan time allows.
#[derive(Debug, Clone, Serialize)]
pub struct JobPlan {
    /// The job id.
    pub id: JobId,
    /// Where this job is declared (the `jobs.<id>:` mapping key).
    #[serde(serialize_with = "crate::json_shape::serialize_span")]
    pub span: Span,
    /// Resolved display name for a non-matrix job (`name:` if given, else
    /// the id). For a matrix job this is the stable job id; each concrete
    /// instance's resolved display name is in [`LegPlan::name`].
    pub name: String,
    /// Direct dependencies, in `needs:` declaration order, deduplicated.
    pub needs: Vec<JobId>,
    /// `wave(j)`: `0` if `needs` is empty, else `1 + max` of its
    /// dependencies' waves.
    pub wave: u32,
    /// Resolved runner image for a **non-matrix** job (`strategy.legs`
    /// empty). `None` when this job has a matrix strategy — see each
    /// [`LegPlan`] instead.
    pub runner: Option<RunnerImage>,
    /// `container:`, pass-through (unevaluated — Phase 2's job).
    pub container: Option<ContainerPlan>,
    /// This job's own `env:` layer, pass-through. It is applied after
    /// [`ExecutionPlan::env`] and before [`StepPlan::env`].
    pub env: IndexMap<String, EnvValue>,
    /// This job's `if:`, resolved for a **non-matrix** job. `None` when
    /// this job has a matrix strategy — see each [`LegPlan`] instead; also
    /// `None` when there is no `if:` at all (in which case
    /// [`JobPlan::implicit_status_gate`] is still meaningful).
    pub condition: Option<Condition>,
    /// `true` iff `if:` (or its absence) contains no status-check function
    /// — the implicit `success()` gate applies. Identical across every leg
    /// of a matrix job (a purely syntactic property of the authored text).
    pub implicit_status_gate: bool,
    /// Statically decided skip, for a **non-matrix** job.
    pub skip: Option<StaticSkip>,
    /// `strategy:`, resolved.
    pub strategy: StrategyPlan,
    /// One entry per [`StrategyPlan::legs`] leg, aligned by index — empty
    /// for a non-matrix job.
    pub legs: Vec<LegPlan>,
    /// `outputs:`, resolved as far as possible for a non-matrix job. Empty
    /// for a matrix job; every concrete instance retains its own output
    /// finalization plan in [`LegPlan::outputs`].
    pub outputs: JobOutputsPlan,
    /// The step sequence for a non-matrix job, in file order. Empty for a
    /// matrix job; each concrete instance has independently planned steps
    /// in [`LegPlan::steps`].
    pub steps: Vec<StepPlan>,
}

/// The per-leg resolved data for a matrix job, aligned with
/// [`StrategyPlan::legs`] by index.
#[derive(Debug, Clone, Serialize)]
pub struct LegPlan {
    /// This instance's display name, resolved against its own `matrix`
    /// context.
    pub name: String,
    /// This leg's resolved runner image.
    pub runner: RunnerImage,
    /// The job-level `if:` result copied to this instance after being
    /// evaluated once, before matrix application. GitHub does not expose
    /// `matrix` to `jobs.<job_id>.if`.
    pub condition: Option<Condition>,
    /// This leg's statically decided skip.
    pub skip: Option<StaticSkip>,
    /// This instance's job-output finalization plan, evaluated against its
    /// own matrix context and retaining runtime dependencies.
    pub outputs: JobOutputsPlan,
    /// This instance's independently planned step sequence.
    pub steps: Vec<StepPlan>,
}

/// A job/leg statically known to be skipped at plan time.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StaticSkip {
    /// Its own `if:` folded to `Static(false)`.
    ConditionFalse,
    /// Sound propagation (design memo §3.3): this job/leg's
    /// [`JobPlan::implicit_status_gate`] is `true`, and a needed job is
    /// itself fully, statically skipped — the implicit `success()` gate
    /// fails before this node's own condition (even a deferred one) is
    /// ever consulted. Never propagated through a node whose condition
    /// contains a status function.
    NeedSkipped {
        /// The skipped dependency.
        need: JobId,
    },
}

/// One step, fully resolved as far as plan time allows.
#[derive(Debug, Clone, Serialize)]
pub struct StepPlan {
    /// `id:`, if given.
    pub id: Option<String>,
    /// Resolved display name: the literal/folded string when statically
    /// known, else the raw authored text (never invented — the same
    /// static/deferred template folding [`crate::outputs::PlannedValue`]
    /// uses).
    pub name: Option<String>,
    /// This step's own `env:` entries (not merged with the outer layers —
    /// a caller combines [`ExecutionPlan::env`], [`JobPlan::env`], then
    /// this layer, with the more specific layer winning), pass-through.
    pub env: IndexMap<String, EnvValue>,
    /// This step's `if:`, resolved.
    pub condition: Option<Condition>,
    /// See [`JobPlan::implicit_status_gate`] — the step-level equivalent.
    pub implicit_status_gate: bool,
    /// Whether this is a `run:` or `uses:` step.
    pub kind: StepKind,
}

/// A step's action, per `PHASE-1-engine-core.md`: "step list with kind
/// (`run` | `uses`)".
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StepKind {
    /// A `run:` step. The script is always raw text (never evaluated here
    /// — it mixes shell code with zero or more `${{ }}` placeholders).
    Run {
        /// The script body, verbatim.
        script: String,
        /// `shell:`, if given.
        shell: Option<String>,
    },
    /// A `uses:` step.
    Uses {
        /// The action reference, verbatim (never an expression in valid
        /// GitHub syntax).
        reference: String,
        /// `with:`, pass-through.
        with: IndexMap<String, EnvValue>,
    },
}

/// Inputs to [`plan`] beyond the workflow and event: local variable
/// overrides and the matrix-leg cap.
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// The already-resolved `vars` context (a flat object) — resolving the
    /// CLI-override/process-env/`.litci/vars` precedence chain is
    /// `greenlit-app`'s job (`PHASE-1-engine-core.md`'s greenlit-app
    /// section); this crate only evaluates against whatever map it is
    /// given.
    pub vars: Value,
    /// The matrix leg cap (design memo §1.2 Phase 4); defaults to
    /// [`DEFAULT_MAX_MATRIX_LEGS`].
    pub max_matrix_legs: usize,
}

impl Default for PlanOptions {
    fn default() -> Self {
        PlanOptions {
            vars: Value::object(vec![]),
            max_matrix_legs: DEFAULT_MAX_MATRIX_LEGS,
        }
    }
}

/// Everything that can go wrong planning a workflow.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// A recognized-but-rejected construct (`concurrency`, `workflow_call`,
    /// `environment`, or a reusable-workflow call job) was present.
    /// `greenlit-workflow` parses these successfully; planning is where
    /// v0's precise rejection happens (`PHASE-1-engine-core.md`).
    #[error("{span}: {name}: not in v0")]
    NotSupportedInV0 {
        /// The construct's name.
        name: &'static str,
        /// Where it appears.
        span: Span,
    },
    /// The `needs` graph could not be built.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// A job's `strategy:` could not be resolved. Boxed per
    /// `clippy::result_large_err`: [`MatrixError`] carries `Span`s and
    /// `Vec<DeferReason>`s, which would otherwise inflate every
    /// `Result<_, PlanError>` return value in this crate.
    #[error("job '{job}': {source}")]
    Matrix {
        /// The job.
        job: JobId,
        /// The underlying failure.
        #[source]
        source: Box<MatrixError>,
    },
    /// A job's (or leg's) `runs-on:` could not be resolved. Boxed for the
    /// same reason as [`PlanError::Matrix`].
    #[error("job '{job}': {source}")]
    Runner {
        /// The job.
        job: JobId,
        /// The underlying failure.
        #[source]
        source: Box<RunnerError>,
    },
    /// A job's or step's `if:`/`env:`/`outputs:`/`name:` could not be
    /// folded. Boxed for the same reason as [`PlanError::Matrix`].
    #[error("job '{job}'{}: {source}", step.as_ref().map(|s| format!(" step '{s}'")).unwrap_or_default())]
    Eval {
        /// The job.
        job: JobId,
        /// The step id, if this happened while planning a step rather than
        /// the job itself.
        step: Option<String>,
        /// The underlying failure.
        #[source]
        source: Box<PartialEvalError>,
    },
    /// A job-level `if:` referenced a context or special function GitHub
    /// does not make available for that workflow key.
    #[error(
        "{span}: job '{job}' `if:` uses {unavailable}, which is not available in `jobs.<job_id>.if` — move this check to a step-level `if:` condition"
    )]
    JobConditionUnavailable {
        /// The job containing the invalid condition.
        job: JobId,
        /// The unavailable context/function, formatted for the diagnostic.
        unavailable: String,
        /// The authored `if:` field's source span.
        span: Span,
    },
    /// A condition or output referenced `needs.<job>.outputs.*`/`.result`
    /// for a job that is not in the referencing job's own `needs:` list —
    /// `needs` context only ever contains direct dependencies, so the
    /// reference could never resolve (design memo §4.3).
    #[error(
        "job '{job}' references needs.{referenced}.* but does not declare `needs: {referenced}`"
    )]
    NeedsReferenceNotDeclared {
        /// The referencing job.
        job: JobId,
        /// The job it wrongly assumed was a direct dependency.
        referenced: JobId,
    },
}

/// Plans `workflow` against `event`, using `options` for local variable
/// overrides. The single entrypoint `greenlit-app` calls.
pub fn plan(
    workflow: &Workflow,
    event: &SyntheticEvent,
    options: &PlanOptions,
) -> Result<ExecutionPlan, PlanError> {
    reject_unsupported_workflow_constructs(workflow)?;

    let graph = build_graph(&workflow.jobs)?;

    let mut lints = Vec::new();
    let mut job_plans: Vec<JobPlan> = Vec::with_capacity(workflow.jobs.len());
    for job in &workflow.jobs {
        reject_unsupported_job_constructs(job)?;
        lint_duplicate_needs(job, &mut lints);
        let job_plan = job::plan_job(workflow, job, event, options, &graph, &mut lints)?;
        job_plans.push(job_plan);
    }

    skip::propagate_static_skip(&graph, &mut job_plans);
    job::lint_matrix_output_collisions(&job_plans, &mut lints);

    let topo_order: Vec<JobId> = graph
        .topo_order()
        .iter()
        .map(|idx| graph.id_of(*idx).clone())
        .collect();

    Ok(ExecutionPlan {
        event_name: event.kind.event_name().to_string(),
        env: crate::pass_through::env_layer_to_map(&workflow.env),
        jobs: job_plans,
        topo_order,
        lints,
    })
}

fn reject_unsupported_workflow_constructs(workflow: &Workflow) -> Result<(), PlanError> {
    if let Some(uc) = &workflow.concurrency {
        return Err(PlanError::NotSupportedInV0 {
            name: uc.name,
            span: uc.location.clone(),
        });
    }
    for trigger in &workflow.on {
        if let greenlit_workflow::model::trigger::Trigger::WorkflowCall(uc) = &trigger.value {
            return Err(PlanError::NotSupportedInV0 {
                name: uc.name,
                span: uc.location.clone(),
            });
        }
    }
    Ok(())
}

fn reject_unsupported_job_constructs(job: &Job) -> Result<(), PlanError> {
    if let Some(uc) = [&job.environment, &job.concurrency, &job.reusable_call]
        .into_iter()
        .flatten()
        .next()
    {
        return Err(PlanError::NotSupportedInV0 {
            name: uc.name,
            span: uc.location.clone(),
        });
    }
    Ok(())
}

fn lint_duplicate_needs(job: &Job, lints: &mut Vec<Lint>) {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for need in &job.needs {
        if !seen.insert(&need.value) {
            lints.push(Lint::duplicate_needs(need.span.clone(), &need.value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condition::PlannedCond;
    use crate::defer::{DeferReason, StepStatusField};
    use crate::event::{EventKind, SyntheticEvent, build_synthetic_event};
    use crate::matrix::MatrixValue;
    use crate::outputs::PlannedValue;
    use crate::pass_through::LiteralValue;
    use std::collections::HashMap as StdHashMap;
    use std::path::Path;

    fn parse_workflow(source: &str) -> greenlit_workflow::model::workflow::Workflow {
        greenlit_workflow::parse_workflow("t.yml", source).expect("workflow should parse")
    }

    fn event_for(
        workflow: &greenlit_workflow::model::workflow::Workflow,
        kind: EventKind,
    ) -> SyntheticEvent {
        build_synthetic_event(
            kind,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            workflow,
            &StdHashMap::new(),
        )
        .expect("repository metadata should produce a synthetic event")
    }

    fn plan_source(source: &str) -> Result<ExecutionPlan, PlanError> {
        plan_source_with(source, EventKind::Push, &PlanOptions::default())
    }

    fn plan_source_with(
        source: &str,
        kind: EventKind,
        options: &PlanOptions,
    ) -> Result<ExecutionPlan, PlanError> {
        let workflow = parse_workflow(source);
        let event = event_for(&workflow, kind);
        plan(&workflow, &event, options)
    }

    fn value_field<'a>(value: &'a Value, name: &str) -> &'a Value {
        match value {
            Value::Object(entries) => entries
                .get(name)
                .unwrap_or_else(|| panic!("missing object field {name}")),
            other => panic!("expected object, got {other:?}"),
        }
    }

    fn string_field<'a>(value: &'a Value, name: &str) -> &'a str {
        match value_field(value, name) {
            Value::String(value) => value,
            other => panic!("expected string field {name}, got {other:?}"),
        }
    }

    fn matrix_string<'a>(leg: &'a crate::matrix::MatrixLeg, name: &str) -> &'a str {
        match leg.values.get(name) {
            Some(MatrixValue::String(value)) => value,
            other => panic!("expected string matrix value {name}, got {other:?}"),
        }
    }

    // GitHub Contexts reference, "matrix context": the context changes for
    // each generated job. The availability table explicitly permits it in
    // job names, job outputs, step names, and step `if`.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#matrix-context
    #[test]
    fn matrix_instances_plan_names_outputs_and_steps_independently() {
        let source = r#"
on: push
jobs:
  test:
    name: "Test ${{ matrix.os }}"
    if: github.event_name == 'push'
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-22.04, ubuntu-24.04]
    outputs:
      selected: ${{ matrix.os }}
      finalized: "${{ matrix.os }}-${{ steps.gated.outputs.value }}"
    steps:
      - id: gated
        name: "Step ${{ matrix.os }}"
        if: matrix.os == 'ubuntu-24.04'
        run: echo test
"#;
        let execution = plan_source(source).expect("matrix should plan");
        let job = &execution.jobs[0];

        assert!(job.strategy.is_matrix);
        assert!(job.steps.is_empty());
        assert!(job.outputs.entries.is_empty());
        assert_eq!(job.legs.len(), 2);

        // Workflow syntax states jobs.<job_id>.if is evaluated before
        // strategy.matrix. Both instances therefore receive the same
        // already-planned condition, while their instance fields below
        // use distinct matrix contexts.
        // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idif
        for leg in &job.legs {
            assert!(matches!(
                leg.condition.as_ref().map(|condition| &condition.eval),
                Some(PlannedCond::Static(true))
            ));
        }

        assert_eq!(job.legs[0].name, "Test ubuntu-22.04");
        assert_eq!(job.legs[1].name, "Test ubuntu-24.04");
        assert_eq!(job.legs[0].runner, RunnerImage::Ubuntu2204);
        assert_eq!(job.legs[1].runner, RunnerImage::Ubuntu2404);
        assert_eq!(
            job.legs[0]
                .outputs
                .entries
                .get("selected")
                .map(|output| &output.value),
            Some(&PlannedValue::Static("ubuntu-22.04".to_string()))
        );
        assert_eq!(
            job.legs[1]
                .outputs
                .entries
                .get("selected")
                .map(|output| &output.value),
            Some(&PlannedValue::Static("ubuntu-24.04".to_string()))
        );
        for (leg, expected_prefix) in [
            (&job.legs[0], "ubuntu-22.04"),
            (&job.legs[1], "ubuntu-24.04"),
        ] {
            match &leg
                .outputs
                .entries
                .get("finalized")
                .expect("per-instance runtime output")
                .value
            {
                PlannedValue::Deferred(deferred) => {
                    assert!(deferred.residual_text.starts_with(expected_prefix));
                    assert!(deferred.defers_on.contains(&DeferReason::StepOutput {
                        step: "gated".to_string(),
                        output: Some("value".to_string()),
                    }));
                }
                other => panic!("expected per-instance deferred output, got {other:?}"),
            }
        }

        assert_eq!(
            job.legs[0].steps[0].name.as_deref(),
            Some("Step ubuntu-22.04")
        );
        assert_eq!(
            job.legs[1].steps[0].name.as_deref(),
            Some("Step ubuntu-24.04")
        );
        assert!(matches!(
            job.legs[0].steps[0]
                .condition
                .as_ref()
                .map(|condition| &condition.eval),
            Some(PlannedCond::Static(false))
        ));
        assert!(matches!(
            job.legs[1].steps[0]
                .condition
                .as_ref()
                .map(|condition| &condition.eval),
            Some(PlannedCond::Static(true))
        ));
    }

    // GitHub's matrix guide defines one job per surviving combination and
    // its "Excluding matrix configurations" section says matching
    // configurations are removed. This pins the degenerate result at zero
    // instances instead of reinterpreting it as a non-matrix job.
    // https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/run-job-variations#excluding-matrix-configurations
    #[test]
    fn fully_excluded_matrix_has_zero_job_instances() {
        let source = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [ubuntu-22.04]
        exclude:
          - os: ubuntu-22.04
    steps:
      - run: echo must-not-be-an-implicit-instance
"#;
        let execution = plan_source(source).expect("zero-instance matrix should plan");
        let job = &execution.jobs[0];

        assert!(job.strategy.is_matrix);
        assert!(job.strategy.legs.is_empty());
        assert!(job.legs.is_empty());
        assert!(job.runner.is_none());
        assert!(job.steps.is_empty());
    }

    // GitHub Context availability: jobs.<job_id>.if permits only github,
    // needs, vars, and inputs. Workflow syntax also states that job `if` is
    // evaluated before matrix application.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idif
    #[test]
    fn unavailable_job_if_contexts_are_hard_errors_with_a_fix() {
        for (condition, expected_name) in [
            ("matrix.os == 'ubuntu-24.04'", "matrix"),
            ("env.DEPLOY == 'yes'", "env"),
        ] {
            let source = format!(
                r#"
on: push
env:
  DEPLOY: yes
jobs:
  test:
    if: {condition}
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [ubuntu-24.04]
    steps:
      - run: echo test
"#
            );
            let error = plan_source(&source).expect_err("invalid job context should fail");
            match &error {
                PlanError::JobConditionUnavailable {
                    unavailable, span, ..
                } => {
                    assert!(unavailable.contains(expected_name));
                    assert!(span.to_string().starts_with("t.yml:"));
                }
                other => panic!("expected context-availability error, got {other:?}"),
            }
            assert!(
                error
                    .to_string()
                    .contains("move this check to a step-level `if:` condition")
            );
        }
    }

    // GitHub Contexts reference documents the fully formed ref, short ref
    // name, and branch/tag type. The trigger table specifies a PR merge ref
    // and the branch/tag selected for workflow_dispatch.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#github-context
    // https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#pull_request
    // https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_dispatch
    #[test]
    fn synthetic_events_populate_top_level_ref_fields_for_each_supported_event() {
        let workflow = parse_workflow(
            r#"
on: [push, pull_request, workflow_dispatch]
jobs:
  test:
    runs-on: ubuntu-latest
    steps: [{run: echo test}]
"#,
        );
        let git = crate::git::collect_git_context(Path::new(env!("CARGO_MANIFEST_DIR")))
            .expect("git metadata should be available");

        let push = event_for(&workflow, EventKind::Push);
        assert_eq!(
            string_field(&push.github, "ref"),
            format!("refs/heads/{}", git.branch)
        );
        assert_eq!(string_field(&push.github, "ref_name"), git.branch);
        assert_eq!(string_field(&push.github, "ref_type"), "branch");
        assert_eq!(
            string_field(value_field(&push.github, "event"), "ref"),
            format!("refs/heads/{}", git.branch)
        );

        let pull_request = event_for(&workflow, EventKind::PullRequest);
        assert_eq!(
            string_field(&pull_request.github, "ref"),
            "refs/pull/1/merge"
        );
        assert_eq!(string_field(&pull_request.github, "ref_name"), "1/merge");
        assert_eq!(string_field(&pull_request.github, "ref_type"), "branch");

        let dispatch = event_for(&workflow, EventKind::WorkflowDispatch);
        assert_eq!(
            string_field(&dispatch.github, "ref"),
            format!("refs/heads/{}", git.branch)
        );
        assert_eq!(string_field(&dispatch.github, "ref_name"), git.branch);
        assert_eq!(string_field(&dispatch.github, "ref_type"), "branch");
        assert_eq!(
            string_field(value_field(&dispatch.github, "event"), "ref"),
            git.branch
        );
    }

    // GitHub's Contexts page uses this exact job-level pattern to select a
    // branch. A synthetic push must therefore make github.ref statically
    // truthy for the checked-out local branch.
    // https://docs.github.com/en/actions/concepts/workflows-and-actions/contexts#determining-when-to-use-contexts
    #[test]
    fn synthetic_push_ref_drives_job_condition() {
        let git = crate::git::collect_git_context(Path::new(env!("CARGO_MANIFEST_DIR")))
            .expect("git metadata should be available");
        let escaped_branch = git.branch.replace('\'', "''");
        let source = format!(
            r#"
on: push
jobs:
  test:
    if: github.ref == 'refs/heads/{escaped_branch}'
    runs-on: ubuntu-latest
    steps: [{{run: echo test}}]
"#
        );
        let execution = plan_source(&source).expect("push ref should plan");
        assert!(matches!(
            execution.jobs[0]
                .condition
                .as_ref()
                .map(|condition| &condition.eval),
            Some(PlannedCond::Static(true))
        ));
    }

    // GitHub exposes needs only after prerequisite jobs finish and steps
    // only as a job executes. Greenlit's Phase-1 contract therefore keeps
    // any authored reference to those contexts runtime-deferred, even when
    // a sibling operand is currently decisive under && or ||.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#needs-context
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#steps-context
    #[test]
    fn short_circuit_folding_never_erases_runtime_dependencies() {
        let source = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps: [{run: echo build}]
  test:
    needs: build
    if: false && needs.build.result == 'success'
    runs-on: ubuntu-latest
    steps:
      - id: first
        run: echo first
      - id: second
        if: true || steps.first.outcome == 'success'
        run: echo second
"#;
        let execution = plan_source(source).expect("deferred conditions should plan");
        let test = &execution.jobs[1];

        match &test.condition.as_ref().expect("job condition").eval {
            PlannedCond::Deferred(deferred) => {
                assert!(deferred.defers_on.contains(&DeferReason::NeedsResult {
                    job: JobId::from("build")
                }))
            }
            other => panic!("expected deferred job condition, got {other:?}"),
        }
        match &test.steps[1]
            .condition
            .as_ref()
            .expect("step condition")
            .eval
        {
            PlannedCond::Deferred(deferred) => {
                assert!(deferred.defers_on.contains(&DeferReason::StepStatus {
                    step: "first".to_string(),
                    field: StepStatusField::Outcome,
                }))
            }
            other => panic!("expected deferred step condition, got {other:?}"),
        }
    }

    // Contexts reference permits env in step `if`, while workflow commands
    // can change environment values for later steps. A non-literal env
    // index is therefore intentionally retained for runtime selection.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#env-context
    // https://docs.github.com/en/actions/using-workflows/workflow-commands-for-github-actions#setting-an-environment-variable
    #[test]
    fn dynamically_indexed_env_access_is_runtime_deferred() {
        let source = r#"
on: push
env:
  TARGET: yes
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - if: env[vars.KEY] == 'yes'
        run: echo selected
"#;
        let options = PlanOptions {
            vars: Value::object(vec![(
                "KEY".to_string(),
                Value::String("TARGET".to_string()),
            )]),
            ..PlanOptions::default()
        };
        let execution =
            plan_source_with(source, EventKind::Push, &options).expect("workflow should plan");

        match &execution.jobs[0].steps[0]
            .condition
            .as_ref()
            .expect("step condition")
            .eval
        {
            PlannedCond::Deferred(deferred) => {
                assert!(deferred.defers_on.contains(&DeferReason::DynamicEnv {
                    name: "*".to_string()
                }))
            }
            other => panic!("expected deferred dynamic env condition, got {other:?}"),
        }
    }

    // Workflow syntax, `env`: the most specific value wins (workflow <
    // job < step), and the env context at a step contains variables from
    // those layers.
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#env
    // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#env-context
    #[test]
    fn execution_plan_retains_and_folds_the_real_env_layer_chain() {
        let source = r#"
on: push
env:
  OUTER: workflow
  SHARED: workflow
jobs:
  test:
    runs-on: ubuntu-latest
    env:
      JOB_ONLY: job
      SHARED: job
    steps:
      - id: inherited
        if: env.OUTER == 'workflow' && env.JOB_ONLY == 'job' && env.SHARED == 'job'
        run: echo inherited
      - id: step
        env:
          COPY: ${{ env.OUTER }}
          SHARED: step
        if: env.COPY == 'workflow' && env.SHARED == 'step'
        run: echo step
"#;
        let execution = plan_source(source).expect("layered env should plan");
        let job = &execution.jobs[0];

        assert_eq!(
            execution.env.get("OUTER"),
            Some(&EnvValue::Literal {
                value: LiteralValue::String("workflow".to_string())
            })
        );
        assert!(!job.env.contains_key("OUTER"));
        assert_eq!(
            job.env.get("JOB_ONLY"),
            Some(&EnvValue::Literal {
                value: LiteralValue::String("job".to_string())
            })
        );
        for step in &job.steps {
            assert!(matches!(
                step.condition.as_ref().map(|condition| &condition.eval),
                Some(PlannedCond::Static(true))
            ));
        }
    }

    // The workflow-syntax matrix examples define exclude as removing
    // combinations and include as augmenting survivors or adding a new
    // combination. This test reaches that algorithm only through plan().
    // https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstrategymatrix
    #[test]
    fn matrix_include_and_exclude_are_visible_through_the_public_plan() {
        let source = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        fruit: [apple, pear]
        os: [ubuntu-22.04, ubuntu-24.04]
        exclude:
          - fruit: pear
            os: ubuntu-22.04
        include:
          - color: green
          - fruit: banana
            os: ubuntu-24.04
    steps: [{run: echo test}]
"#;
        let execution = plan_source(source).expect("matrix should plan");
        let job = &execution.jobs[0];

        assert_eq!(job.strategy.legs.len(), 4);
        assert_eq!(matrix_string(&job.strategy.legs[0], "fruit"), "apple");
        assert_eq!(matrix_string(&job.strategy.legs[1], "fruit"), "apple");
        assert_eq!(matrix_string(&job.strategy.legs[2], "fruit"), "pear");
        assert_eq!(matrix_string(&job.strategy.legs[3], "fruit"), "banana");
        for leg in &job.strategy.legs[..3] {
            assert_eq!(matrix_string(leg, "color"), "green");
        }
    }

    #[test]
    fn matrix_diagnostics_are_exposed_by_plan() {
        let too_many = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [ubuntu-22.04, ubuntu-24.04]
    steps: [{run: echo test}]
"#;
        let options = PlanOptions {
            max_matrix_legs: 1,
            ..PlanOptions::default()
        };
        assert!(matches!(
            plan_source_with(too_many, EventKind::Push, &options),
            Err(PlanError::Matrix { .. })
        ));

        let empty_exclude = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [ubuntu-24.04]
        exclude:
          - {}
    steps: [{run: echo test}]
"#;
        assert!(matches!(
            plan_source(empty_exclude),
            Err(PlanError::Matrix { .. })
        ));

        let dead_exclude = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [ubuntu-24.04]
        exclude:
          - os: ubuntu-22.04
    steps: [{run: echo test}]
"#;
        let execution = plan_source(dead_exclude).expect("dead exclude is a lint");
        assert_eq!(execution.lints.len(), 1);
        assert_eq!(execution.lints[0].kind, crate::lints::LintKind::DeadExclude);
    }

    #[test]
    fn deferred_output_behavior_and_json_shape_are_exposed_by_plan() {
        let source = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    outputs:
      version: v1.4-${{ steps.meta.outputs.sha }}
    steps:
      - id: meta
        run: echo meta
"#;
        let execution = plan_source(source).expect("output should plan");
        match &execution.jobs[0]
            .outputs
            .entries
            .get("version")
            .expect("declared output")
            .value
        {
            PlannedValue::Deferred(deferred) => {
                assert_eq!(deferred.residual_text, "v1.4-${{ steps.meta.outputs.sha }}");
            }
            other => panic!("expected deferred output, got {other:?}"),
        }

        let json = serde_json::to_value(&execution).expect("plan should serialize");
        assert_eq!(
            json["jobs"][0]["outputs"]["entries"]["version"]["evaluation"],
            "deferred"
        );
    }

    #[test]
    fn cycle_error_names_only_real_dependency_edges() {
        let source = r#"
on: push
jobs:
  a:
    needs: b
    runs-on: ubuntu-latest
    steps: [{run: echo a}]
  b:
    needs: [b, c]
    runs-on: ubuntu-latest
    steps: [{run: echo b}]
  c:
    needs: a
    runs-on: ubuntu-latest
    steps: [{run: echo c}]
"#;
        let error = plan_source(source).expect_err("cycle should fail");
        match &error {
            PlanError::Graph(GraphError::Cycles(cycles)) => assert_eq!(
                cycles[0].members,
                vec![JobId::from("a"), JobId::from("b"), JobId::from("c")]
            ),
            other => panic!("expected named cycle, got {other:?}"),
        }
        assert_eq!(error.to_string(), "dependency cycle: a -> b -> c -> a");
    }

    #[test]
    fn planner_errors_and_skip_propagation_remain_public_behaviors() {
        let unsupported = r#"
on: push
jobs:
  build:
    runs-on: windows-latest
    steps: [{run: echo}]
"#;
        assert!(matches!(
            plan_source(unsupported),
            Err(PlanError::Runner { .. })
        ));

        let workflow_call = r#"
on:
  workflow_call: {}
jobs:
  build:
    runs-on: ubuntu-latest
    steps: [{run: echo}]
"#;
        assert!(matches!(
            plan_source(workflow_call),
            Err(PlanError::NotSupportedInV0 {
                name: "workflow_call",
                ..
            })
        ));

        let missing_need = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps: [{run: echo}]
  test:
    runs-on: ubuntu-latest
    if: needs.build.result == 'success'
    steps: [{run: echo}]
"#;
        assert!(matches!(
            plan_source(missing_need),
            Err(PlanError::NeedsReferenceNotDeclared { .. })
        ));

        let skipped_need = r#"
on: push
jobs:
  build:
    if: false
    runs-on: ubuntu-latest
    steps: [{run: echo}]
  test:
    needs: build
    runs-on: ubuntu-latest
    steps: [{run: echo}]
"#;
        let execution = plan_source(skipped_need).expect("skip propagation should plan");
        assert_eq!(
            execution.jobs[1].skip,
            Some(StaticSkip::NeedSkipped {
                need: JobId::from("build")
            })
        );
    }
}
