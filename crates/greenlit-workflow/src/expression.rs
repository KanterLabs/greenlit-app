//! Workflow template parsing around `greenlit-expr`'s inner grammar.

use crate::error::ParseError;
use crate::span::Span;
use greenlit_expr::Expr;

/// The context and special-function policy for one expression-capable
/// workflow key. This table transcribes GitHub's "Context availability"
/// table; ordinary expression functions are available everywhere, while
/// `hashFiles` and the four status functions are site-restricted:
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability>.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ExpressionPolicy {
    WorkflowEnv,
    JobIf,
    JobStrategy,
    JobStrategyContext,
    JobEnv,
    JobDefaultsRun,
    JobOutputs,
    Container,
    ContainerCredentials,
    ContainerEnv,
    Step,
    StepIf,
}

const WORKFLOW_ENV: &[&str] = &["github", "secrets", "inputs", "vars"];
const JOB_IF: &[&str] = &["github", "needs", "vars", "inputs"];
const JOB_STRATEGY: &[&str] = &["github", "needs", "vars", "inputs"];
const JOB_STRATEGY_CONTEXT: &[&str] = &["github", "needs", "strategy", "matrix", "vars", "inputs"];
const JOB_ENV: &[&str] = &[
    "github", "needs", "strategy", "matrix", "vars", "secrets", "inputs",
];
const JOB_DEFAULTS_RUN: &[&str] = &[
    "github", "needs", "strategy", "matrix", "env", "vars", "inputs",
];
const JOB_OUTPUTS: &[&str] = &[
    "github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "secrets", "steps",
    "inputs",
];
const CONTAINER: &[&str] = &["github", "needs", "strategy", "matrix", "vars", "inputs"];
const CONTAINER_CREDENTIALS: &[&str] = &[
    "github", "needs", "strategy", "matrix", "env", "vars", "secrets", "inputs",
];
const CONTAINER_ENV: &[&str] = &[
    "github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "secrets", "inputs",
];
const STEP: &[&str] = &[
    "github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "secrets", "steps",
    "inputs",
];
const STEP_IF: &[&str] = &[
    "github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "steps", "inputs",
];

impl ExpressionPolicy {
    fn contexts(self) -> &'static [&'static str] {
        match self {
            Self::WorkflowEnv => WORKFLOW_ENV,
            Self::JobIf => JOB_IF,
            Self::JobStrategy => JOB_STRATEGY,
            Self::JobStrategyContext => JOB_STRATEGY_CONTEXT,
            Self::JobEnv => JOB_ENV,
            Self::JobDefaultsRun => JOB_DEFAULTS_RUN,
            Self::JobOutputs => JOB_OUTPUTS,
            Self::Container => CONTAINER,
            Self::ContainerCredentials => CONTAINER_CREDENTIALS,
            Self::ContainerEnv => CONTAINER_ENV,
            Self::Step => STEP,
            Self::StepIf => STEP_IF,
        }
    }

    fn allows_hash_files(self) -> bool {
        matches!(self, Self::Step | Self::StepIf)
    }

    fn allows_status_function(self) -> bool {
        matches!(self, Self::JobIf | Self::StepIf)
    }
}

/// Locate the outer `}}`, ignoring delimiter text inside expression string
/// literals. GitHub expressions allow only single-quoted strings and escape
/// a literal quote by doubling it (`''`); double-quoted strings are invalid:
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#literals>.
///
/// `greenlit-expr`'s public API parses complete expression bodies but does
/// not expose lexer token boundaries, so this state machine performs only
/// wrapper delimiting.
pub(crate) fn find_closing_delimiter(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    let mut in_string = false;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            if in_string && bytes.get(index + 1) == Some(&b'\'') {
                index += 2;
                continue;
            }
            in_string = !in_string;
            index += 1;
            continue;
        }
        if !in_string && bytes[index..].starts_with(b"}}") {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Return the body when `text` consists of exactly one wrapped expression,
/// allowing only surrounding whitespace.
pub(crate) fn single_expression_body(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let after_open = trimmed.strip_prefix("${{")?;
    let end = find_closing_delimiter(after_open)?;
    if !after_open[end + 2..].trim().is_empty() {
        return None;
    }
    Some(after_open[..end].trim())
}

/// Parse and site-validate every `${{ ... }}` segment in a template scalar.
pub(crate) fn validate_template(
    text: &str,
    span: &Span,
    context: &str,
    policy: ExpressionPolicy,
) -> Result<(), ParseError> {
    for expr in parse_template(text, span, context)? {
        validate_site(&expr, span, context, policy)?;
    }
    Ok(())
}

/// Parse and site-validate an `if:` scalar. GitHub treats an unwrapped
/// `if:` value as one expression; when `${{` is present, normal template
/// segmentation applies.
pub(crate) fn validate_if(
    text: &str,
    span: &Span,
    context: &str,
    policy: ExpressionPolicy,
) -> Result<(), ParseError> {
    if text.contains("${{") {
        validate_template(text, span, context, policy)
    } else {
        let expr = parse_body(text, span, context)?;
        validate_site(&expr, span, context, policy)
    }
}

/// Reject a template opener at a workflow key that does not permit
/// expressions. `${{` begins template parsing regardless of quoting at the
/// YAML layer, matching the runner's `TemplateReader.ParseScalar` behavior.
pub(crate) fn reject_template(text: &str, span: &Span, context: &str) -> Result<(), ParseError> {
    if text.contains("${{") {
        Err(ParseError::Expression {
            span: span.clone(),
            context: context.to_owned(),
            message: "expressions are not allowed at this workflow key".to_owned(),
        })
    } else {
        Ok(())
    }
}

/// Parse all wrapped expression segments without applying a workflow-key
/// policy. Static extraction uses this after normal workflow parsing, while
/// retaining a real error path for callers that mutate the public model.
pub(crate) fn parse_template(
    text: &str,
    span: &Span,
    context: &str,
) -> Result<Vec<Expr>, ParseError> {
    let mut expressions = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        let after_open = &rest[start + 3..];
        let end = find_closing_delimiter(after_open).ok_or_else(|| ParseError::Expression {
            span: span.clone(),
            context: context.to_owned(),
            message: "`${{` is not closed by a quote-aware `}}` delimiter".to_owned(),
        })?;
        expressions.push(parse_body(after_open[..end].trim(), span, context)?);
        rest = &after_open[end + 2..];
    }
    Ok(expressions)
}

/// Parse one already-delimited expression body and attach its workflow span.
pub(crate) fn parse_body(body: &str, span: &Span, context: &str) -> Result<Expr, ParseError> {
    greenlit_expr::parse(body).map_err(|error| ParseError::Expression {
        span: span.clone(),
        context: context.to_owned(),
        message: error.to_string(),
    })
}

fn validate_site(
    expr: &Expr,
    span: &Span,
    context: &str,
    policy: ExpressionPolicy,
) -> Result<(), ParseError> {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) => Ok(()),
        Expr::NamedValue(name) => {
            if policy
                .contexts()
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name))
            {
                Ok(())
            } else {
                Err(ParseError::Expression {
                    span: span.clone(),
                    context: context.to_owned(),
                    message: format!(
                        "context '{name}' is not available here; available contexts: {}",
                        policy.contexts().join(", ")
                    ),
                })
            }
        }
        Expr::Call { name, args } => {
            let is_hash_files = name.eq_ignore_ascii_case("hashFiles");
            let is_status = ["always", "cancelled", "failure", "success"]
                .iter()
                .any(|status| status.eq_ignore_ascii_case(name));
            if (is_hash_files && !policy.allows_hash_files())
                || (is_status && !policy.allows_status_function())
            {
                return Err(ParseError::Expression {
                    span: span.clone(),
                    context: context.to_owned(),
                    message: format!("function '{name}' is not available here"),
                });
            }
            for arg in args {
                validate_site(arg, span, context, policy)?;
            }
            Ok(())
        }
        Expr::Index { target, index } => {
            validate_site(target, span, context, policy)?;
            validate_site(index, span, context, policy)
        }
        Expr::Wildcard { target } | Expr::Not(target) => {
            validate_site(target, span, context, policy)
        }
        Expr::Binary { lhs, rhs, .. } => {
            validate_site(lhs, span, context, policy)?;
            validate_site(rhs, span, context, policy)
        }
    }
}
