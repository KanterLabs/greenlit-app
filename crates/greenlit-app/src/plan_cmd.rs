//! `litci plan`: parse a workflow, resolve local variables and the
//! synthetic trigger event, assemble the resolved
//! [`greenlit_engine::ExecutionPlan`], and render it -- tree to stdout by
//! default, stable JSON with `--json`. Stage timings go to stderr and one
//! NDJSON record is appended per invocation, success or failure
//! (`PHASE-1-engine-core.md` greenlit-app/greenlit-metrics sections;
//! `AGENTS.md` Metrics: "every `plan` or `run` invocation appends one NDJSON
//! record").

use std::collections::HashMap;
use std::io::Write;

use greenlit_engine::{PlanOptions, build_synthetic_event, plan, validate_v0_support};
use greenlit_metrics::{Invocation, MetricsStore};

use crate::cli::PlanArgs;
use crate::{errors, render, vars, workflow_discovery};

pub(crate) fn run(args: PlanArgs) -> anyhow::Result<()> {
    let invocation = Invocation::start("plan");
    let result = invocation.with_timing_subscriber(|| execute(&args, &invocation));

    // Stage timings and one metrics record are emitted for *every*
    // invocation, not only a successful one -- a failed plan's partial
    // stage durations (e.g. how long parsing took before a schema error)
    // are still useful, and `litci stats` should see an honest history of
    // every attempt. A successful command is not reported when its required
    // local record could not be persisted.
    let record = invocation.finish();
    let timings_result = render::diagnostics::render_timings(&record, &mut std::io::stderr())
        .map_err(|error| {
            anyhow::anyhow!(
                "could not render stage timings: {error}\n  fix: ensure standard error is writable, then retry"
            )
        });
    let metrics_result = MetricsStore::open_default()
        .and_then(|store| store.append(&record))
        .map_err(|error| errors::metrics_error(&error));

    let mut failure = result.err();
    if let Err(error) = timings_result {
        append_failure(&mut failure, error);
    }
    if let Err(error) = metrics_result {
        append_failure(&mut failure, error);
    }
    failure.map_or(Ok(()), Err)
}

fn append_failure(failure: &mut Option<anyhow::Error>, additional: anyhow::Error) {
    *failure = Some(match failure.take() {
        Some(primary) => anyhow::anyhow!("{primary}\n{additional}"),
        None => additional,
    });
}

fn execute(args: &PlanArgs, invocation: &Invocation) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().map_err(|error| {
        anyhow::anyhow!(
            "could not determine the current directory: {error}\n  fix: change to an accessible repository directory, then retry"
        )
    })?;
    let repo_root = greenlit_engine::git::find_repository_root(&cwd)
        .map_err(|error| errors::event_error(&greenlit_engine::EventError::Git(error)))?;
    let workflow_path =
        workflow_discovery::resolve_workflow_path(args.workflow.as_deref(), &cwd, &repo_root)
            .map_err(|s| anyhow::anyhow!(s))?;

    let workflow = invocation
        .time_stage("parse", || {
            greenlit_workflow::parse_workflow_file_with_name(
                &workflow_path.read_path,
                workflow_path.source_name.as_str(),
            )
        })
        .map_err(|e| errors::parse_error(&e))?;
    validate_v0_support(&workflow).map_err(|error| errors::plan_error(&error))?;

    let (plan_options, event) = (|| -> anyhow::Result<_> {
        let extraction = greenlit_workflow::extract_static(&workflow)
            .map_err(|error| errors::parse_error(&error))?;
        let dotenv_vars = vars::read_dotenv_vars(&repo_root).map_err(|s| anyhow::anyhow!(s))?;
        let vars_value = vars::resolve_vars(
            &extraction,
            &args.vars,
            dotenv_vars.as_deref().unwrap_or_default(),
            dotenv_vars.is_some(),
        )
        .map_err(|failure| errors::vars_resolution(&failure))?;
        let plan_options = PlanOptions {
            vars: vars_value,
            max_matrix_legs: greenlit_engine::DEFAULT_MAX_MATRIX_LEGS,
        };
        let event_kind: greenlit_engine::EventKind = args.event.into();
        let dispatch_inputs: HashMap<String, String> = args.inputs.iter().cloned().collect();
        let event = build_synthetic_event(event_kind, &repo_root, &workflow, &dispatch_inputs)
            .map_err(|e| errors::event_error(&e))?;
        Ok((plan_options, event))
    })()?;

    let execution_plan =
        plan(&workflow, &event, &plan_options).map_err(|e| errors::plan_error(&e))?;

    if args.json {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        serde_json::to_writer_pretty(&mut handle, &execution_plan).map_err(|error| {
            anyhow::anyhow!(
                "could not write the JSON plan to stdout: {error}\n  fix: ensure stdout is writable, then retry"
            )
        })?;
        handle.write_all(b"\n").map_err(|error| {
            anyhow::anyhow!(
                "could not finish writing the JSON plan to stdout: {error}\n  fix: ensure stdout is writable, then retry"
            )
        })?;
    } else {
        render::tree::render(&execution_plan, &mut std::io::stdout()).map_err(|error| {
            anyhow::anyhow!(
                "could not write the plan to stdout: {error}\n  fix: ensure stdout is writable, then retry"
            )
        })?;
    }
    render::diagnostics::render_lints(&execution_plan.lints, &mut std::io::stderr()).map_err(
        |error| {
            anyhow::anyhow!(
                "could not write plan diagnostics to stderr: {error}\n  fix: ensure stderr is writable, then retry"
            )
        },
    )?;

    Ok(())
}
