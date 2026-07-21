//! Static extraction: every `secrets.*`/`vars.*` reference, every dynamic
//! use of the `vars` context, every `uses:` reference, and every
//! `runs-on` value in a parsed workflow — the exact list
//! `PHASE-1-engine-core.md`'s greenlit-workflow section asks for, needed
//! before `litci run`/`litci plan` can resolve secrets/variables
//! (`greenlit-v0-spec.md` "Secrets/vars") or validate runner labels
//! ("Stable Linux x64 runners").
//!
//! # Extraction strategy
//!
//! For each `${{ ... }}` occurrence, a delimiter scan that follows the
//! expression grammar's single-quoted-string rule finds the real closing
//! `}}`; this module then calls
//! [`greenlit_expr::parse`] on the interior text and walks the resulting
//! [`greenlit_expr::ast::Expr`] tree (the same shape of walk
//! `greenlit_expr::ast::expr_calls_status_function` does), looking for
//! `Expr::Index { target: Expr::NamedValue(root), index: Expr::Str(name) }`
//! where `root` case-insensitively matches `"secrets"`/`"vars"`. Both the
//! dot form (`secrets.FOO`) and the bracket form (`secrets['FOO']`) parse to
//! this exact same AST shape (`a.b` is defined as sugar for `a['b']`), so
//! both are found uniformly with no separate dot/bracket-handling code
//! path, and expressions like `secrets[format('{0}', 'FOO')]` are correctly
//! *not* mistaken for a literal reference to a secret named `FOO` (the
//! index is a `Call`, not a `Str`, so it falls through to the dynamic-vars/
//! opaque-index handling below instead).
//!
//! This superseded an earlier interim version of this module that did its
//! own best-effort raw-text token scanning, written before `greenlit-expr`
//! exposed a usable public lexer/parser entry point. Now that both crates
//! are stable, this module depends on the real grammar directly instead.
//!
//! Expression syntax is never best-effort: [`crate::parse_workflow`]
//! validates every modeled site before returning, and this API retains a
//! checked [`Result`] for callers that mutate the public model afterward.

use crate::error::ParseError;
use crate::expression::{parse_body, parse_template};
use crate::model::job::{ContainerSpec, Job, Matrix, MatrixSource, RunsOn, Strategy};
use crate::model::step::{Step, StepAction};
use crate::model::value::{ScalarOrExpr, YamlValue};
use crate::model::workflow::Workflow;
use crate::span::{Span, Spanned};
use greenlit_expr::ast::Expr;
use std::collections::BTreeMap;

/// Everything statically discoverable from a parsed workflow without
/// evaluating any expression.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StaticExtraction {
    /// Every literal `secrets.NAME` / `secrets['NAME']` reference found,
    /// mapped to every location it appears at.
    pub secrets: BTreeMap<String, Vec<Span>>,
    /// Every literal `vars.NAME` / `vars['NAME']` reference found, mapped
    /// to every location it appears at.
    pub vars: BTreeMap<String, Vec<Span>>,
    /// Whether a use of the complete `vars` map appears anywhere: a
    /// non-literal lookup, object-filter wildcard, or whole-map reference.
    /// Per `greenlit-v0-spec.md` "Secrets/vars", this requires a complete
    /// locally supplied map because the exact names needed are not all
    /// statically knowable.
    pub has_dynamic_vars_lookup: bool,
    /// One span for every dynamic use of the `vars` context, including a
    /// non-literal index (`vars[matrix.key]`), an object-filter wildcard
    /// (`vars.*` / `vars[*]`), or the whole map (`toJSON(vars)`). Multiple
    /// lookups in one scalar intentionally contribute duplicate spans.
    pub dynamic_vars: Vec<Span>,
    /// Every `uses:` reference, in document order.
    pub uses: Vec<Spanned<String>>,
    /// Every `runs-on` value, in document order.
    pub runs_on: Vec<Spanned<String>>,
}

/// Scan a parsed workflow for every `secrets.*`/`vars.*` reference, dynamic
/// `vars` use, `uses:` reference, and `runs-on` value.
///
/// # Errors
/// Returns [`ParseError::Expression`] if a caller mutated a parsed model to
/// contain an invalid expression. Workflows returned directly by
/// [`crate::parse_workflow`] have already passed the same grammar checks.
pub fn extract_static(workflow: &Workflow) -> Result<StaticExtraction, ParseError> {
    let mut out = StaticExtraction::default();
    for (_, v) in &workflow.env {
        scan_scalar_or_expr(v, &mut out)?;
    }
    for job in &workflow.jobs {
        scan_job(job, &mut out)?;
    }
    Ok(out)
}

fn scan_job(job: &Job, out: &mut StaticExtraction) -> Result<(), ParseError> {
    if let Some(name) = &job.name {
        // `vars` is explicitly available in `jobs.<job_id>.name`:
        // https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability
        scan_wrapped_text(&name.value, &name.span, out)?;
    }
    if let Some(cond) = &job.if_condition {
        scan_if_condition(cond, out)?;
    }
    for (_, v) in &job.outputs {
        scan_wrapped_text(&v.value, &v.span, out)?;
    }
    for (_, v) in &job.env {
        scan_scalar_or_expr(v, out)?;
    }
    if let Some(runs_on) = &job.runs_on {
        for label in runs_on_values(&runs_on.value) {
            scan_wrapped_text(&label.value, &label.span, out)?;
            out.runs_on.push(label);
        }
    }
    if let Some(strategy) = &job.strategy {
        scan_strategy(&strategy.value, out)?;
    }
    if let Some(defaults) = &job.defaults
        && let Some(run) = &defaults.value.run
    {
        if let Some(shell) = &run.value.shell {
            scan_wrapped_text(&shell.value, &shell.span, out)?;
        }
        if let Some(working_directory) = &run.value.working_directory {
            scan_scalar_or_expr(working_directory, out)?;
        }
    }
    for (_, svc) in &job.services {
        scan_container(&svc.value, out)?;
    }
    if let Some(container) = &job.container {
        scan_container(&container.value, out)?;
    }
    for step in &job.steps {
        scan_step(step, out)?;
    }
    Ok(())
}

fn runs_on_values(r: &RunsOn) -> Vec<Spanned<String>> {
    match r {
        RunsOn::Label(s) => vec![s.clone()],
        RunsOn::Labels(list) => list.clone(),
        RunsOn::Group { group, labels } => {
            let mut v = Vec::new();
            if let Some(g) = group {
                v.push(g.clone());
            }
            v.extend(labels.iter().cloned());
            v
        }
    }
}

fn scan_strategy(strategy: &Strategy, out: &mut StaticExtraction) -> Result<(), ParseError> {
    if let Some(m) = &strategy.matrix {
        match &m.value {
            MatrixSource::Expression(e) => scan_wrapped_text(&e.value, &e.span, out)?,
            MatrixSource::Inline(matrix) => scan_matrix(matrix, out)?,
        }
    }
    if let Some(f) = &strategy.fail_fast {
        scan_scalar_or_expr(f, out)?;
    }
    if let Some(mp) = &strategy.max_parallel {
        scan_scalar_or_expr(mp, out)?;
    }
    Ok(())
}

fn scan_matrix(matrix: &Matrix, out: &mut StaticExtraction) -> Result<(), ParseError> {
    for (_, values) in &matrix.axes {
        for v in values {
            scan_yaml_value(v, out)?;
        }
    }
    for entry in matrix.include.iter().chain(matrix.exclude.iter()) {
        for (_, v) in &entry.value {
            scan_yaml_value(v, out)?;
        }
    }
    Ok(())
}

fn scan_container(c: &ContainerSpec, out: &mut StaticExtraction) -> Result<(), ParseError> {
    scan_scalar_or_expr(&c.image, out)?;
    if let Some(creds) = &c.credentials {
        if let Some(u) = &creds.value.username {
            scan_scalar_or_expr(u, out)?;
        }
        if let Some(p) = &creds.value.password {
            scan_scalar_or_expr(p, out)?;
        }
    }
    for (_, v) in &c.env {
        scan_scalar_or_expr(v, out)?;
    }
    for p in &c.ports {
        scan_scalar_or_expr(p, out)?;
    }
    for v in &c.volumes {
        scan_scalar_or_expr(v, out)?;
    }
    if let Some(o) = &c.options {
        scan_scalar_or_expr(o, out)?;
    }
    Ok(())
}

fn scan_step(step: &Step, out: &mut StaticExtraction) -> Result<(), ParseError> {
    if let Some(cond) = &step.if_condition {
        scan_if_condition(cond, out)?;
    }
    if let Some(name) = &step.name {
        scan_scalar_or_expr(name, out)?;
    }
    for (_, v) in &step.env {
        scan_scalar_or_expr(v, out)?;
    }
    if let Some(wd) = &step.working_directory {
        scan_scalar_or_expr(wd, out)?;
    }
    if let Some(coe) = &step.continue_on_error {
        scan_scalar_or_expr(coe, out)?;
    }
    if let Some(tm) = &step.timeout_minutes {
        scan_scalar_or_expr(tm, out)?;
    }
    match &step.action {
        StepAction::Run { script, shell } => {
            scan_wrapped_text(&script.value, &script.span, out)?;
            if let Some(sh) = shell {
                scan_wrapped_text(&sh.value, &sh.span, out)?;
            }
        }
        StepAction::Uses { reference, with } => {
            out.uses.push(reference.clone());
            for (_, v) in with {
                scan_scalar_or_expr(v, out)?;
            }
        }
    }
    Ok(())
}

fn scan_scalar_or_expr(
    v: &Spanned<ScalarOrExpr>,
    out: &mut StaticExtraction,
) -> Result<(), ParseError> {
    if let ScalarOrExpr::Expression(text) = &v.value {
        scan_wrapped_text(text, &v.span, out)?;
    }
    Ok(())
}

fn scan_yaml_value(v: &Spanned<YamlValue>, out: &mut StaticExtraction) -> Result<(), ParseError> {
    match &v.value {
        YamlValue::Scalar(s) => {
            if let ScalarOrExpr::Expression(text) = s {
                scan_wrapped_text(text, &v.span, out)?;
            }
        }
        YamlValue::Sequence(items) => {
            for item in items {
                scan_yaml_value(item, out)?;
            }
        }
        YamlValue::Mapping(entries) => {
            for (_, value) in entries {
                scan_yaml_value(value, out)?;
            }
        }
    }
    Ok(())
}

/// `if:` text is always an expression, with or without the `${{ }}`
/// wrapper, as documented for workflow conditionals at
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idif>.
/// Unlike other fields, an unwrapped `if:`
/// value must be scanned as one whole expression body rather than only
/// searching for `${{ }}` occurrences within it.
fn scan_if_condition(cond: &Spanned<String>, out: &mut StaticExtraction) -> Result<(), ParseError> {
    if cond.value.contains("${{") {
        scan_wrapped_text(&cond.value, &cond.span, out)
    } else {
        let expr = parse_body(&cond.value, &cond.span, "static extraction")?;
        walk_expr(&expr, &cond.span, out);
        Ok(())
    }
}

/// Find every `${{ ... }}` occurrence in `text` and scan its interior.
fn scan_wrapped_text(
    text: &str,
    span: &Span,
    out: &mut StaticExtraction,
) -> Result<(), ParseError> {
    for expr in parse_template(text, span, "static extraction")? {
        walk_expr(&expr, span, out);
    }
    Ok(())
}

fn record(out: &mut StaticExtraction, is_secrets: bool, name: &str, span: &Span) {
    let map = if is_secrets {
        &mut out.secrets
    } else {
        &mut out.vars
    };
    map.entry(name.to_owned()).or_default().push(span.clone());
}

fn record_dynamic_vars(out: &mut StaticExtraction, span: &Span) {
    out.has_dynamic_vars_lookup = true;
    out.dynamic_vars.push(span.clone());
}

/// Walks a parsed expression tree for `secrets.*`/`vars.*` references —
/// see this module's doc comment for exactly which AST shape is matched
/// and why the dot/bracket forms need no separate handling.
fn walk_expr(expr: &Expr, span: &Span, out: &mut StaticExtraction) {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) => {}
        Expr::NamedValue(root) => {
            if root.eq_ignore_ascii_case("vars") {
                record_dynamic_vars(out, span);
            }
        }
        Expr::Not(inner) => walk_expr(inner, span, out),
        Expr::Wildcard { target } => {
            if matches!(target.as_ref(), Expr::NamedValue(root) if root.eq_ignore_ascii_case("vars"))
            {
                record_dynamic_vars(out, span);
            } else {
                walk_expr(target, span, out);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, span, out);
            walk_expr(rhs, span, out);
        }
        Expr::Call { args, .. } => {
            for a in args {
                walk_expr(a, span, out);
            }
        }
        Expr::Index { target, index } => {
            if let Expr::NamedValue(root) = target.as_ref() {
                let is_secrets = root.eq_ignore_ascii_case("secrets");
                let is_vars = root.eq_ignore_ascii_case("vars");
                if is_secrets || is_vars {
                    match index.as_ref() {
                        Expr::Str(name) => record(out, is_secrets, name, span),
                        _ => {
                            if is_vars {
                                record_dynamic_vars(out, span);
                            }
                            // A non-literal index (e.g. `secrets[format(...)]`)
                            // can itself reference `secrets.*`/`vars.*`;
                            // keep walking it rather than dropping it.
                            walk_expr(index, span, out);
                        }
                    }
                    return;
                }
            }
            walk_expr(target, span, out);
            walk_expr(index, span, out);
        }
    }
}
