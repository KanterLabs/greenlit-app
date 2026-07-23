//! Formatting of planned values shared across human tree sections.

use greenlit_engine::{
    Condition, DeferReason, Evaluation, Planned, PlannedCond, RunnerPlan, StaticSkip, StatusFn,
    StepStatusField,
};

use crate::render::terminal::inline_escape;

pub(super) fn display_planned_string(value: &Planned<String>) -> String {
    inline_escape(match &value.evaluation {
        Evaluation::Static(value) => value,
        Evaluation::Deferred(deferred) => &deferred.residual_text,
    })
}

pub(super) fn format_planned_string(value: &Planned<String>) -> String {
    format_planned(value, |value| format!("{value:?}"))
}

pub(super) fn format_planned_bool(value: &Planned<bool>) -> String {
    format_planned(value, ToString::to_string)
}

pub(super) fn format_planned_number(value: &Planned<f64>) -> String {
    format_planned(value, |value| {
        if value.fract() == 0.0 {
            format!("{value:.0}")
        } else {
            value.to_string()
        }
    })
}

pub(super) fn format_planned<T>(
    planned: &Planned<T>,
    format_static: impl FnOnce(&T) -> String,
) -> String {
    let rendered = match &planned.evaluation {
        Evaluation::Static(value) => {
            format!("static({}) <- {}", format_static(value), planned.source)
        }
        Evaluation::Deferred(deferred) => format!(
            "deferred <- {} (defers on: {})",
            deferred.residual_text,
            format_defer_reasons(&deferred.defers_on)
        ),
    };
    inline_escape(&rendered)
}

pub(super) fn format_runner(runner: &RunnerPlan) -> String {
    let rendered = match &runner.evaluation {
        Evaluation::Static(image) => {
            format!("static({}) <- {}", image.image_identifier(), runner.source)
        }
        Evaluation::Deferred(deferred) => format!(
            "deferred <- {} (defers on: {})",
            deferred.residual_text,
            format_defer_reasons(&deferred.defers_on)
        ),
    };
    inline_escape(&rendered)
}

pub(super) fn format_condition(
    condition: Option<&Condition>,
    implicit_status_gate: bool,
) -> String {
    let status_gate = if implicit_status_gate {
        " -- implicit success() gate"
    } else {
        ""
    };
    let rendered = match condition {
        None => format!("(none){status_gate}"),
        Some(condition) => match &condition.eval {
            PlannedCond::Static(value) => {
                format!("static({value}) <- {}{status_gate}", condition.source)
            }
            PlannedCond::Deferred(deferred) => format!(
                "deferred <- {} (defers on: {}){status_gate}",
                deferred.residual_text,
                format_defer_reasons(&deferred.defers_on)
            ),
        },
    };
    inline_escape(&rendered)
}

pub(super) fn skip_suffix(skip: Option<&StaticSkip>) -> String {
    let rendered = match skip {
        None => String::new(),
        Some(StaticSkip::ConditionFalse) => " [skipped: condition is statically false]".to_string(),
        Some(StaticSkip::NeedSkipped { need }) => {
            format!(" [skipped: dependency '{need}' is skipped]")
        }
    };
    inline_escape(&rendered)
}

pub(super) fn format_defer_reasons(reasons: &[DeferReason]) -> String {
    inline_escape(
        &reasons
            .iter()
            .map(format_defer_reason)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn format_defer_reason(reason: &DeferReason) -> String {
    match reason {
        DeferReason::NeedsOutput {
            job,
            output: Some(output),
        } => format!("needs.{job}.outputs.{output}"),
        DeferReason::NeedsOutput { job, output: None } => format!("needs.{job}.outputs"),
        DeferReason::NeedsResult { job } => format!("needs.{job}.result"),
        DeferReason::StepOutput {
            step,
            output: Some(output),
        } => format!("steps.{step}.outputs.{output}"),
        DeferReason::StepOutput { step, output: None } => format!("steps.{step}.outputs"),
        DeferReason::StepStatus { step, field } => format!(
            "steps.{step}.{}",
            match field {
                StepStatusField::Outcome => "outcome",
                StepStatusField::Conclusion => "conclusion",
            }
        ),
        DeferReason::StatusFn(function) => format!(
            "{}()",
            match function {
                StatusFn::Success => "success",
                StatusFn::Failure => "failure",
                StatusFn::Cancelled => "cancelled",
                StatusFn::Always => "always",
            }
        ),
        DeferReason::HashFiles => "hashFiles(...)".to_string(),
        DeferReason::DynamicEnv { name } => format!("env.{name}"),
        DeferReason::GithubContext {
            property: Some(property),
        } => format!("github.{property}"),
        DeferReason::GithubContext { property: None } => "github".to_string(),
        DeferReason::RunnerContext => "runner.*".to_string(),
        DeferReason::JobContext => "job.*".to_string(),
        DeferReason::MatrixContext => "matrix.*".to_string(),
        DeferReason::StrategyContext => "strategy.*".to_string(),
        DeferReason::SecretsContext => "secrets.*".to_string(),
        DeferReason::NeedsContextWhole => "needs".to_string(),
        DeferReason::StepsContextWhole => "steps".to_string(),
    }
}
