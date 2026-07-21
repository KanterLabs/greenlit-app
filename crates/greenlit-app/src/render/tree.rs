//! The default human-readable plan rendering: an indented tree of jobs,
//! matrix legs, outputs, and steps, written to stdout
//! (`PHASE-1-engine-core.md`: "human-readable tree output by default,
//! stable JSON with --json").
//!
//! A matrix job's outputs/steps/display-name live on each
//! [`greenlit_engine::LegPlan`] instead of the job itself (the job-level
//! fields are empty for a matrix job -- see [`greenlit_engine::JobPlan`]'s
//! doc comments), so this renderer branches on
//! [`greenlit_engine::StrategyPlan::is_matrix`] to decide whether to walk
//! the job-level or the per-leg fields.

use std::io::Write;

use greenlit_engine::{
    Condition, DeferReason, EnvValue, ExecutionPlan, JobOutputsPlan, JobPlan, LiteralValue,
    PlannedCond, PlannedOutput, PlannedValue, StaticSkip, StatusFn, StepKind, StepPlan,
    StepStatusField,
};

/// Renders the whole plan as an indented tree.
pub(crate) fn render(plan: &ExecutionPlan, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "event: {}", plan.event_name)?;
    if plan.env.iter().next().is_some() {
        writeln!(out, "env: {}", format_env_map(&plan.env))?;
    }
    let order: Vec<&str> = plan.topo_order.iter().map(|id| id.0.as_str()).collect();
    writeln!(out, "topo order: {}", order.join(" -> "))?;
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
            .map(|n| n.0.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    writeln!(
        out,
        "  {} [wave {}] needs: {}{}",
        job.name,
        job.wave,
        needs,
        skip_suffix(job.skip.as_ref())
    )?;
    if job.name != job.id.0 {
        writeln!(out, "    id: {}", job.id)?;
    }

    if job.strategy.is_matrix {
        render_strategy(job, out)?;
    } else {
        if let Some(runner) = &job.runner {
            writeln!(out, "    runner: {}", runner.image_identifier())?;
        }
        render_condition_line(
            job.condition.as_ref(),
            job.implicit_status_gate,
            out,
            "    ",
        )?;
        render_outputs(&job.outputs, out, "    ")?;
        writeln!(out, "    steps:")?;
        for step in &job.steps {
            render_step(step, out, "      ")?;
        }
    }
    Ok(())
}

fn skip_suffix(skip: Option<&StaticSkip>) -> String {
    match skip {
        None => String::new(),
        Some(StaticSkip::ConditionFalse) => " [skipped: condition is statically false]".to_string(),
        Some(StaticSkip::NeedSkipped { need }) => {
            format!(" [skipped: dependency '{need}' is skipped]")
        }
    }
}

fn render_condition_line(
    condition: Option<&Condition>,
    implicit_status_gate: bool,
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    match condition {
        None => writeln!(
            out,
            "{indent}if: (none){}",
            if implicit_status_gate {
                " -- implicit success() gate"
            } else {
                ""
            }
        ),
        Some(c) => writeln!(out, "{indent}if: {}", format_condition(c)),
    }
}

fn format_condition(c: &Condition) -> String {
    match &c.eval {
        PlannedCond::Static(b) => format!("static({b}) <- {}", c.source),
        PlannedCond::Deferred(d) => format!(
            "deferred <- {} (defers on: {})",
            d.residual_text,
            format_defer_reasons(&d.defers_on)
        ),
    }
}

fn format_defer_reasons(reasons: &[DeferReason]) -> String {
    reasons
        .iter()
        .map(format_defer_reason)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_defer_reason(r: &DeferReason) -> String {
    match r {
        DeferReason::NeedsOutput {
            job,
            output: Some(o),
        } => format!("needs.{job}.outputs.{o}"),
        DeferReason::NeedsOutput { job, output: None } => format!("needs.{job}.outputs"),
        DeferReason::NeedsResult { job } => format!("needs.{job}.result"),
        DeferReason::StepOutput {
            step,
            output: Some(o),
        } => format!("steps.{step}.outputs.{o}"),
        DeferReason::StepOutput { step, output: None } => format!("steps.{step}.outputs"),
        DeferReason::StepStatus { step, field } => format!(
            "steps.{step}.{}",
            match field {
                StepStatusField::Outcome => "outcome",
                StepStatusField::Conclusion => "conclusion",
            }
        ),
        DeferReason::StatusFn(f) => format!(
            "{}()",
            match f {
                StatusFn::Success => "success",
                StatusFn::Failure => "failure",
                StatusFn::Cancelled => "cancelled",
                StatusFn::Always => "always",
            }
        ),
        DeferReason::HashFiles => "hashFiles(...)".to_string(),
        DeferReason::DynamicEnv { name } => format!("env.{name}"),
        DeferReason::RunnerContext => "runner.*".to_string(),
        DeferReason::JobContext => "job.*".to_string(),
        DeferReason::SecretsContext => "secrets.*".to_string(),
        DeferReason::NeedsContextWhole => "needs".to_string(),
        DeferReason::StepsContextWhole => "steps".to_string(),
    }
}

/// Renders `strategy:` plus each leg's independently planned
/// name/runner/if/outputs/steps -- a matrix job carries none of those at
/// the job level (see [`greenlit_engine::JobPlan`]'s doc comments).
fn render_strategy(job: &JobPlan, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(
        out,
        "    strategy: matrix fail-fast={} max-parallel={} legs={}",
        job.strategy.fail_fast,
        job.strategy
            .max_parallel
            .map(|n| n.to_string())
            .unwrap_or_else(|| "(none)".to_string()),
        job.strategy.legs.len()
    )?;
    for (leg, leg_plan) in job.strategy.legs.iter().zip(&job.legs) {
        writeln!(
            out,
            "      leg {} \"{}\": runner={}{}",
            leg.index,
            leg_plan.name,
            leg_plan.runner.image_identifier(),
            skip_suffix(leg_plan.skip.as_ref())
        )?;
        render_condition_line(
            leg_plan.condition.as_ref(),
            job.implicit_status_gate,
            out,
            "        ",
        )?;
        render_outputs(&leg_plan.outputs, out, "        ")?;
        writeln!(out, "        steps:")?;
        for step in &leg_plan.steps {
            render_step(step, out, "          ")?;
        }
    }
    Ok(())
}

fn render_outputs(
    outputs: &JobOutputsPlan,
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    if outputs.entries.is_empty() {
        return writeln!(out, "{indent}outputs: (none)");
    }
    writeln!(out, "{indent}outputs:")?;
    for (name, output) in &outputs.entries {
        writeln!(out, "{indent}  {name}: {}", format_output(output))?;
    }
    Ok(())
}

fn format_output(o: &PlannedOutput) -> String {
    match &o.value {
        PlannedValue::Static(s) => format!("static(\"{s}\") <- {}", o.source),
        PlannedValue::Deferred(d) => format!(
            "deferred <- {} (defers on: {})",
            d.residual_text,
            format_defer_reasons(&d.defers_on)
        ),
    }
}

fn render_step(step: &StepPlan, out: &mut impl Write, indent: &str) -> std::io::Result<()> {
    let label = step
        .id
        .as_deref()
        .map(|id| format!("[{id}] "))
        .unwrap_or_default();
    let name = step.name.as_deref().unwrap_or("");
    match &step.kind {
        StepKind::Run { script, shell } => {
            let title = if name.is_empty() {
                first_line(script)
            } else {
                name.to_string()
            };
            let shell_note = shell
                .as_deref()
                .map(|s| format!(" (shell: {s})"))
                .unwrap_or_default();
            writeln!(out, "{indent}{label}run{shell_note} -- {title}")?;
        }
        StepKind::Uses { reference, with } => {
            let name_note = if name.is_empty() {
                String::new()
            } else {
                format!(" -- {name}")
            };
            writeln!(out, "{indent}{label}uses {reference}{name_note}")?;
            if with.iter().next().is_some() {
                writeln!(out, "{indent}  with: {}", format_env_map(with))?;
            }
        }
    }
    let condition_indent = format!("{indent}  ");
    render_condition_line(
        step.condition.as_ref(),
        step.implicit_status_gate,
        out,
        &condition_indent,
    )?;
    if step.env.iter().next().is_some() {
        writeln!(out, "{indent}  env: {}", format_env_map(&step.env))?;
    }
    Ok(())
}

fn first_line(script: &str) -> String {
    script.lines().next().unwrap_or("").to_string()
}

fn format_env_map<'a>(entries: impl IntoIterator<Item = (&'a String, &'a EnvValue)>) -> String {
    entries
        .into_iter()
        .map(|(k, v)| format!("{k}={}", format_env_value(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_env_value(v: &EnvValue) -> String {
    match v {
        EnvValue::Literal { value } => format_literal(value),
        EnvValue::Expression { source } => source.clone(),
    }
}

fn format_literal(v: &LiteralValue) -> String {
    match v {
        LiteralValue::Null => "null".to_string(),
        LiteralValue::Bool(b) => b.to_string(),
        LiteralValue::Number(n) => greenlit_expr::value::format_g15(*n),
        LiteralValue::String(s) => s.clone(),
    }
}
