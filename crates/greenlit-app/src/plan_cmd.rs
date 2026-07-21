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

use greenlit_engine::{PlanOptions, build_synthetic_event, plan};
use greenlit_metrics::{Invocation, MetricsStore};

use crate::cli::PlanArgs;
use crate::{errors, render, vars, workflow_discovery};

pub(crate) fn run(args: PlanArgs) -> anyhow::Result<()> {
    let invocation = Invocation::start("plan");
    let result = execute(&args, &invocation);

    // Stage timings and one metrics record are emitted for *every*
    // invocation, not only a successful one -- a failed plan's partial
    // stage durations (e.g. how long parsing took before a schema error)
    // are still useful, and `litci stats` should see an honest history of
    // every attempt. These are secondary effects: a failure here is
    // reported as a warning rather than replacing `result`, so a metrics
    // I/O glitch can never mask the real plan outcome.
    let record = invocation.finish();
    if let Err(e) = render::diagnostics::render_timings(&record, &mut std::io::stderr()) {
        eprintln!("warning: could not render stage timings: {e}");
    }
    if let Err(e) = MetricsStore::open_default().and_then(|store| store.append(&record)) {
        eprintln!("warning: could not record metrics: {e}");
    }

    result
}

fn execute(args: &PlanArgs, invocation: &Invocation) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let workflow_path = workflow_discovery::resolve_workflow_path(args.workflow.as_deref(), &cwd)
        .map_err(|s| anyhow::anyhow!(s))?;

    let workflow = invocation
        .time_stage("parse", || {
            greenlit_workflow::parse_workflow_file(&workflow_path)
        })
        .map_err(|e| errors::parse_error(&e))?;

    // "eval" spans everything needed to build the contexts `plan()` folds
    // against (resolved `vars`, the synthetic `github`/`inputs` event) --
    // `greenlit-engine::plan()` itself performs its own internal
    // condition/output partial evaluation as part of assembling the plan
    // (there is no separately exposed "evaluate" entry point in Phase 1), so
    // that work is timed as part of the "plan" stage below instead.
    let (plan_options, event) = invocation.time_stage("eval", || -> anyhow::Result<_> {
        let extraction = greenlit_workflow::extract_static(&workflow);
        let dotenv_path = vars::default_vars_file(&cwd);
        let dotenv_vars = vars::read_dotenv_vars(&dotenv_path).map_err(|s| anyhow::anyhow!(s))?;
        let vars_value = vars::resolve_vars(&extraction, &args.vars, &dotenv_vars)
            .map_err(|missing| errors::missing_vars(&missing))?;
        let plan_options = PlanOptions {
            vars: vars_value,
            max_matrix_legs: greenlit_engine::DEFAULT_MAX_MATRIX_LEGS,
        };
        let event_kind: greenlit_engine::EventKind = args.event.into();
        let dispatch_inputs: HashMap<String, String> = HashMap::new();
        let event = build_synthetic_event(event_kind, &cwd, &workflow, &dispatch_inputs)
            .map_err(|e| errors::event_error(&e))?;
        Ok((plan_options, event))
    })?;

    let execution_plan = invocation
        .time_stage("plan", || plan(&workflow, &event, &plan_options))
        .map_err(|e| errors::plan_error(&e))?;

    if args.json {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        serde_json::to_writer_pretty(&mut handle, &execution_plan)?;
        handle.write_all(b"\n")?;
    } else {
        render::tree::render(&execution_plan, &mut std::io::stdout())?;
        render::diagnostics::render_lints(&execution_plan.lints, &mut std::io::stderr())?;
    }

    Ok(())
}
