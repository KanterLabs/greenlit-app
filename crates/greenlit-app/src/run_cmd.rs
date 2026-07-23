//! `litci run`: resolve the execution plan (as `litci plan` does), detect the
//! container engine, then execute the plan in isolated containers — one per
//! job, one exec per step, sequentially in DAG order — streaming live logs and
//! printing an end-of-run table.
//!
//! `PHASE-2-execution.md` objective: "`litci run` executes a shell-only
//! workflow green, end to end." Planning/evaluation semantics are the engine's;
//! container execution is `greenlit-runtime`'s executor. This module wires the
//! two, records the run's metrics (`AGENTS.md` Metrics: "every `plan` or `run`
//! invocation appends one NDJSON record"), and renders the human summary.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::process::ExitCode;

use greenlit_engine::execution::env::RunnerEnv;
use greenlit_engine::git::{collect_git_context, find_repository_root};
use greenlit_engine::{
    Conclusion, DEFAULT_MAX_MATRIX_LEGS, ExecutionPlan, JobId, PlanOptions, build_synthetic_event,
    plan, validate_v0_support,
};
use greenlit_expr::Value;
use greenlit_metrics::{Invocation, MetricsStore};
use greenlit_runtime::{
    DockerEngine, EngineState, RunConfig, RunReport, StepReport, SystemProber, detect, run_plan,
    validate_host,
};

use crate::cli::RunArgs;
use crate::{errors, render, vars, workflow_discovery};

/// Run the command, returning the process exit code (a failed workflow run
/// exits non-zero without an error, since its table is the real output).
pub(crate) fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let invocation = Invocation::start("run");
    let outcome = invocation.with_timing_subscriber(|| execute(&args, &invocation));

    let record = invocation.finish();
    let _ = render::diagnostics::render_timings(&record, &mut io::stderr());
    let metrics_result = MetricsStore::open_default()
        .and_then(|store| store.append(&record))
        .map_err(|error| errors::metrics_error(&error));

    let code = outcome?;
    metrics_result?;
    Ok(code)
}

fn execute(args: &RunArgs, invocation: &Invocation) -> anyhow::Result<ExitCode> {
    validate_host().map_err(|host| anyhow::anyhow!("{host}\n  fix: {}", host.fix()))?;

    let cwd = std::env::current_dir().map_err(|error| {
        anyhow::anyhow!(
            "could not determine the current directory: {error}\n  fix: change to an accessible repository directory, then retry"
        )
    })?;
    let repo_root = find_repository_root(&cwd)
        .map_err(|error| errors::event_error(&greenlit_engine::EventError::Git(error)))?;
    let workflow_path =
        workflow_discovery::resolve_workflow_path(args.workflow.as_deref(), &cwd, &repo_root)
            .map_err(|message| anyhow::anyhow!(message))?;

    let workflow = invocation
        .time_stage("parse", || {
            greenlit_workflow::parse_workflow_file_with_name(
                &workflow_path.read_path,
                workflow_path.source_name.as_str(),
            )
        })
        .map_err(|error| errors::parse_error(&error))?;
    validate_v0_support(&workflow).map_err(|error| errors::plan_error(&error))?;

    let extraction = greenlit_workflow::extract_static(&workflow)
        .map_err(|error| errors::parse_error(&error))?;
    let dotenv_vars =
        vars::read_dotenv_vars(&repo_root).map_err(|message| anyhow::anyhow!(message))?;
    let vars_value = vars::resolve_vars(
        &extraction,
        &args.vars,
        dotenv_vars.as_deref().unwrap_or_default(),
        dotenv_vars.is_some(),
    )
    .map_err(|failure| errors::vars_resolution(&failure))?;

    let event_kind: greenlit_engine::EventKind = args.event.into();
    let dispatch_inputs: HashMap<String, String> = args.inputs.iter().cloned().collect();
    let event = build_synthetic_event(event_kind, &repo_root, &workflow, &dispatch_inputs)
        .map_err(|error| errors::event_error(&error))?;
    let plan_options = PlanOptions {
        vars: vars_value.clone(),
        max_matrix_legs: DEFAULT_MAX_MATRIX_LEGS,
    };
    let execution_plan = invocation
        .time_stage("plan", || plan(&workflow, &event, &plan_options))
        .map_err(|error| errors::plan_error(&error))?;
    let execution_plan = match &args.job {
        Some(job) => prune_to_job(&execution_plan, job)?,
        None => execution_plan,
    };

    let git = collect_git_context(&repo_root)
        .map_err(|error| errors::event_error(&greenlit_engine::EventError::Git(error)))?;
    let workflow_name = workflow
        .name
        .as_ref()
        .map(|name| name.value.clone())
        .unwrap_or_else(|| workflow_path.source_name.clone());
    let repo_leaf = git
        .repository
        .rsplit('/')
        .next()
        .unwrap_or(&git.repository)
        .to_string();
    let workspace = format!("/home/runner/work/{repo_leaf}/{repo_leaf}");

    let config = RunConfig {
        repo_host_path: repo_root.clone(),
        workspace: workspace.clone(),
        strategy: args.isolation.into(),
        runner_env: build_runner_env(&git, event_kind, workflow_name, workspace),
        github: event.github.clone(),
        vars: vars_value,
        inputs: event.inputs.clone(),
        secrets: Value::object(vec![]),
        initial_masks: args
            .secrets
            .iter()
            .map(|(_, value)| value.clone())
            .collect(),
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            anyhow::anyhow!(
                "could not start the async runtime: {error}\n  fix: retry; if it persists, file an issue"
            )
        })?;

    let engine = invocation.time_stage("detection", || runtime.block_on(connect_engine()))?;

    // `Stdout` (not a lock guard) is `Send`, which the executor's streaming sink
    // requires; each write still locks internally.
    let mut out = io::stdout();
    let report = runtime
        .block_on(run_plan(&engine, &execution_plan, &config, &mut out))
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    for job in &report.jobs {
        for step in &job.steps {
            invocation.record_step_duration(job.id.clone(), step.label.clone(), step.duration);
        }
    }

    render_run_table(&report, &mut out).map_err(|error| {
        anyhow::anyhow!(
            "could not write the run summary: {error}\n  fix: ensure stdout is writable"
        )
    })?;

    Ok(if report.failed() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Detect and connect to the container engine, mapping every failure state to a
/// message plus the one action that fixes it (`AGENTS.md` UX invariant).
async fn connect_engine() -> anyhow::Result<DockerEngine> {
    match detect(&SystemProber::new()).await {
        EngineState::Available { endpoint } => {
            DockerEngine::connect(&endpoint).map_err(|error| {
                anyhow::anyhow!(
                    "reached the container engine at {} but could not open a client: {error}\n  fix: restart the daemon, then retry",
                    endpoint.describe()
                )
            })
        }
        EngineState::DaemonStopped(fix) => {
            Err(anyhow::anyhow!("{}\n  fix: {}", fix.message, fix.action))
        }
        EngineState::NotInstalled(fix) => {
            Err(anyhow::anyhow!("{}\n  fix: {}", fix.message, fix.action))
        }
    }
}

/// Build the runner/`github` environment template from local git metadata.
fn build_runner_env(
    git: &greenlit_engine::git::GitContext,
    event_kind: greenlit_engine::EventKind,
    workflow_name: String,
    workspace: String,
) -> RunnerEnv {
    RunnerEnv {
        workflow: workflow_name,
        repository: git.repository.clone(),
        repository_owner: git.repository_owner.clone(),
        sha: git.sha.clone(),
        ref_full: format!("refs/heads/{}", git.branch),
        ref_name: git.branch.clone(),
        ref_type: "branch".to_string(),
        event_name: event_kind.event_name().to_string(),
        actor: git.actor.clone(),
        job: String::new(),
        run_id: "1".to_string(),
        run_number: "1".to_string(),
        run_attempt: "1".to_string(),
        workspace,
        runner_name: "greenlit".to_string(),
        runner_temp: "/tmp".to_string(),
        runner_tool_cache: "/opt/hostedtoolcache".to_string(),
    }
}

/// Prune `plan` to the requested job and its transitive `needs:` closure.
fn prune_to_job(plan: &ExecutionPlan, job: &str) -> anyhow::Result<ExecutionPlan> {
    let target = JobId(job.to_string());
    if !plan.jobs.iter().any(|candidate| candidate.id == target) {
        anyhow::bail!(
            "no job '{job}' in this workflow\n  fix: pass -j with a job id defined under `jobs:`"
        );
    }
    let mut keep: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<JobId> = VecDeque::from([target]);
    while let Some(id) = queue.pop_front() {
        if !keep.insert(id.0.clone()) {
            continue;
        }
        if let Some(job_plan) = plan.jobs.iter().find(|candidate| candidate.id == id) {
            for need in &job_plan.needs {
                queue.push_back(need.clone());
            }
        }
    }
    let mut pruned = plan.clone();
    pruned
        .jobs
        .retain(|candidate| keep.contains(&candidate.id.0));
    pruned.topo_order.retain(|id| keep.contains(&id.0));
    Ok(pruned)
}

/// Render the end-of-run table: each job, its steps' outcomes and durations,
/// then the overall result.
fn render_run_table(report: &RunReport, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "\nRun summary")?;
    for job in &report.jobs {
        writeln!(out, "  {} [{}]", job.display, job.result.as_github_str())?;
        for step in &job.steps {
            writeln!(
                out,
                "    {} {}  {}",
                step_sigil(step),
                step.label,
                format_duration(step)
            )?;
        }
    }
    writeln!(out, "\nResult: {}", report.overall.as_github_str())?;
    Ok(())
}

/// A short sigil for a step's conclusion in the summary table.
fn step_sigil(step: &StepReport) -> &'static str {
    match step.conclusion {
        Conclusion::Success => "\u{2713}",
        Conclusion::Failure => "\u{2717}",
        Conclusion::Cancelled => "\u{29b8}",
        Conclusion::Skipped => "\u{2013}",
    }
}

/// Format a step's duration, or a dash for a step that did not run.
fn format_duration(step: &StepReport) -> String {
    if !step.ran {
        return "—".to_string();
    }
    let millis = step.duration.as_millis();
    if millis >= 1000 {
        format!("{:.1}s", step.duration.as_secs_f64())
    } else {
        format!("{millis}ms")
    }
}
