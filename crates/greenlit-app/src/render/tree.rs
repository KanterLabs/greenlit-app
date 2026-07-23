//! The default human-readable execution-plan tree.
//!
//! Unlike the compact job heading, every context-sensitive field below is
//! rendered with its explicit static/deferred state. This keeps the default
//! `litci plan` output useful for fidelity debugging without changing the
//! stable `--json` schema.

use std::io::Write;

use greenlit_engine::{
    ExecutionPlan, JobPlan, LegPlan, MatrixPlan, StepKind, StepPlan, StrategyControl,
};

mod fields;
mod format;

use fields::{
    render_container, render_defaults, render_env, render_outputs, render_permissions,
    render_services,
};
use format::{
    display_planned_string, format_condition, format_defer_reasons, format_planned_bool,
    format_planned_number, format_planned_string, format_runner, skip_suffix,
};

/// Renders the whole plan as an indented tree.
pub(crate) fn render(plan: &ExecutionPlan, out: &mut impl Write) -> std::io::Result<()> {
    super::terminal::render_sanitized(out, |buffer| render_unescaped(plan, buffer))
}

fn render_unescaped(plan: &ExecutionPlan, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "plan schema: {}", plan.schema_version)?;
    writeln!(out, "event: {}", plan.event_name)?;
    match &plan.run_name {
        Some(run_name) => writeln!(out, "run name: {}", format_planned_string(run_name))?,
        None => writeln!(out, "run name: (event default)")?,
    }
    render_env("env", &plan.env, out, "")?;
    render_defaults(&plan.defaults, out, "")?;
    render_permissions(plan.permissions.as_ref(), out, "")?;
    let order = plan
        .topo_order
        .iter()
        .map(|id| id.0.as_str())
        .collect::<Vec<_>>();
    writeln!(
        out,
        "topo order: {}",
        super::terminal::inline_escape(&order.join(" -> "))
    )?;
    writeln!(out)?;
    writeln!(out, "jobs:")?;
    for job in &plan.jobs {
        render_job(job, out)?;
    }
    Ok(())
}

fn render_job(job: &JobPlan, out: &mut impl Write) -> std::io::Result<()> {
    let needs = if job.needs.is_empty() {
        "(none)".to_string()
    } else {
        job.needs
            .iter()
            .map(|need| need.0.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    writeln!(
        out,
        "  {} [wave {}] needs: {}{}",
        display_planned_string(&job.name),
        job.wave,
        needs,
        skip_suffix(job.skip.as_ref())
    )?;
    writeln!(
        out,
        "    id: {}",
        super::terminal::inline_escape(&job.id.to_string())
    )?;
    writeln!(out, "    name: {}", format_planned_string(&job.name))?;
    render_permissions(job.permissions.as_ref(), out, "    ")?;

    if job.strategy.is_matrix() {
        render_strategy(job, out)?;
        if job.strategy.is_matrix_deferred() {
            if let Some(runner) = &job.runner {
                writeln!(out, "    runner template: {}", format_runner(runner))?;
            }
            render_instance_fields(
                job.container.as_ref(),
                &job.services,
                &job.env,
                &job.defaults,
                out,
                "    ",
            )?;
            render_condition(job, out, "    ")?;
            render_outputs(&job.outputs, out, "    ")?;
            render_steps(&job.steps, out, "    ", "step template")?;
        }
    } else {
        if let Some(runner) = &job.runner {
            writeln!(out, "    runner: {}", format_runner(runner))?;
        }
        render_instance_fields(
            job.container.as_ref(),
            &job.services,
            &job.env,
            &job.defaults,
            out,
            "    ",
        )?;
        render_condition(job, out, "    ")?;
        render_outputs(&job.outputs, out, "    ")?;
        render_steps(&job.steps, out, "    ", "steps")?;
    }
    Ok(())
}

fn render_condition(job: &JobPlan, out: &mut impl Write, indent: &str) -> std::io::Result<()> {
    let condition = format_condition(job.condition.as_ref(), job.implicit_status_gate);
    writeln!(out, "{indent}if: {condition}")
}

fn render_strategy(job: &JobPlan, out: &mut impl Write) -> std::io::Result<()> {
    let fail_fast = format_strategy_bool(&job.strategy.fail_fast);
    let max_parallel = job
        .strategy
        .max_parallel
        .as_ref()
        .map_or_else(|| "(job total)".to_string(), format_strategy_number);
    if let Some(MatrixPlan::Deferred { expressions, .. }) = &job.strategy.matrix {
        writeln!(
            out,
            "    strategy: matrix deferred fail-fast={fail_fast} max-parallel={max_parallel}"
        )?;
        for expression in expressions {
            writeln!(
                out,
                "      {}: deferred <- {} (defers on: {})",
                super::terminal::inline_escape(&expression.path),
                super::terminal::inline_escape(&expression.residual),
                format_defer_reasons(&expression.defers_on)
            )?;
        }
        return Ok(());
    }

    writeln!(
        out,
        "    strategy: matrix fail-fast={fail_fast} max-parallel={max_parallel} legs={}",
        job.strategy.legs().len()
    )?;
    for (leg, leg_plan) in job.strategy.legs().iter().zip(&job.legs) {
        render_leg(leg.index, leg_plan, job.implicit_status_gate, out)?;
    }
    Ok(())
}

fn format_strategy_bool(value: &StrategyControl<bool>) -> String {
    match value {
        StrategyControl::Static(value) => format!("static({value})"),
        StrategyControl::Deferred(value) => format_planned_bool(value),
    }
}

fn format_strategy_number(value: &StrategyControl<std::num::NonZeroU32>) -> String {
    match value {
        StrategyControl::Static(value) => format!("static({value})"),
        StrategyControl::Deferred(value) => format::format_planned(value, ToString::to_string),
    }
}

fn render_leg(
    index: usize,
    leg: &LegPlan,
    implicit_status_gate: bool,
    out: &mut impl Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "      leg {index} \"{}\"{}",
        display_planned_string(&leg.name),
        skip_suffix(leg.skip.as_ref())
    )?;
    writeln!(out, "        name: {}", format_planned_string(&leg.name))?;
    writeln!(out, "        runner: {}", format_runner(&leg.runner))?;
    render_instance_fields(
        leg.container.as_ref(),
        &leg.services,
        &leg.env,
        &leg.defaults,
        out,
        "        ",
    )?;
    writeln!(
        out,
        "        if: {}",
        format_condition(leg.condition.as_ref(), implicit_status_gate)
    )?;
    render_outputs(&leg.outputs, out, "        ")?;
    render_steps(&leg.steps, out, "        ", "steps")
}

fn render_instance_fields<'a>(
    container: Option<&'a greenlit_engine::ContainerPlan>,
    services: impl IntoIterator<Item = (&'a String, &'a greenlit_engine::ContainerPlan)>,
    env: impl IntoIterator<Item = (&'a String, &'a greenlit_engine::EnvValue)>,
    defaults: &'a greenlit_engine::RunDefaultsPlan,
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    render_container("container", container, out, indent)?;
    render_services(services, out, indent)?;
    render_env("env", env, out, indent)?;
    render_defaults(defaults, out, indent)
}

fn render_steps(
    steps: &[StepPlan],
    out: &mut impl Write,
    indent: &str,
    label: &str,
) -> std::io::Result<()> {
    if steps.is_empty() {
        return writeln!(out, "{indent}{label}: (none)");
    }
    writeln!(out, "{indent}{label}:")?;
    for step in steps {
        render_step(step, out, &format!("{indent}  "))?;
    }
    Ok(())
}

fn render_step(step: &StepPlan, out: &mut impl Write, indent: &str) -> std::io::Result<()> {
    let id = step.id.as_deref().unwrap_or("(none)");
    writeln!(
        out,
        "{indent}step [id: {}]",
        super::terminal::inline_escape(id)
    )?;
    match &step.name {
        Some(name) => writeln!(out, "{indent}  name: {}", format_planned_string(name))?,
        None => writeln!(out, "{indent}  name: (none)")?,
    }
    writeln!(
        out,
        "{indent}  if: {}",
        format_condition(step.condition.as_ref(), step.implicit_status_gate)
    )?;
    match &step.kind {
        StepKind::Run { script, shell } => {
            writeln!(out, "{indent}  kind: run")?;
            writeln!(out, "{indent}  script: {}", format_planned_string(script))?;
            match shell {
                Some(shell) => writeln!(out, "{indent}  shell: {}", format_planned_string(shell))?,
                None => writeln!(out, "{indent}  shell: (default)")?,
            }
        }
        StepKind::Uses { reference, with } => {
            writeln!(out, "{indent}  kind: uses")?;
            writeln!(out, "{indent}  reference: static({reference:?})")?;
            render_env("with", with, out, &format!("{indent}  "))?;
        }
    }
    render_env("env", &step.env, out, &format!("{indent}  "))?;
    match &step.working_directory {
        Some(value) => writeln!(
            out,
            "{indent}  working-directory: {}",
            format_planned_string(value)
        )?,
        None => writeln!(out, "{indent}  working-directory: (default)")?,
    }
    match &step.continue_on_error {
        Some(value) => writeln!(
            out,
            "{indent}  continue-on-error: {}",
            format_planned_bool(value)
        )?,
        None => writeln!(out, "{indent}  continue-on-error: (default false)")?,
    }
    match &step.timeout_minutes {
        Some(value) => writeln!(
            out,
            "{indent}  timeout-minutes: {}",
            format_planned_number(value)
        )?,
        None => writeln!(out, "{indent}  timeout-minutes: (default)")?,
    }
    Ok(())
}
