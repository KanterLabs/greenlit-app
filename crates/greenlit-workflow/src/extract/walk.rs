//! AST walking for static workflow-reference extraction.

use std::io;
use std::path::Path;
use std::sync::Arc;

use greenlit_expr::value::to_display_string;
use greenlit_expr::{
    Context, EntryKind, EvaluationOptions, Expr, HashFilesFs, OpenedDir, Value,
    WORKFLOW_TEMPLATE_MAX_MEMORY_BYTES, evaluate_with_options,
};

use super::StaticExtraction;
use crate::Span;

/// One authored, statically identifiable
/// `needs.<job>.outputs.<name>` reference.
#[derive(Debug, Clone, PartialEq)]
pub struct NeedsOutputReference {
    /// Job containing the reference.
    pub referencing_job: String,
    /// Direct-dependency-shaped job key as authored or computed.
    pub referenced_job: String,
    /// Output key as authored or computed.
    pub output: String,
    /// Scalar containing this occurrence.
    pub span: Span,
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

/// Walks a parsed expression tree for static context references. Dot access
/// is already index sugar in this AST, so literal and bracket forms share
/// one path. A context-free computed index is evaluated with the real
/// expression evaluator; any index containing a context root or filesystem/
/// status function stays intentionally unknown.
pub(super) fn walk_expr(expr: &Expr, span: &Span, out: &mut StaticExtraction) {
    if let Some((referenced_job, output)) = needs_output_path(expr) {
        out.needs_outputs.push(NeedsOutputReference {
            referencing_job: String::new(),
            referenced_job,
            output,
            span: span.clone(),
        });
    }

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
            for argument in args {
                walk_expr(argument, span, out);
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

fn needs_output_path(expr: &Expr) -> Option<(String, String)> {
    let path = static_context_path(expr)?;
    if path.len() < 4
        || !path[0].eq_ignore_ascii_case("needs")
        || !path[2].eq_ignore_ascii_case("outputs")
    {
        return None;
    }
    Some((path[1].clone(), path[3].clone()))
}

fn static_context_path(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::NamedValue(root) => Some(vec![root.clone()]),
        Expr::Index { target, index } => {
            let mut path = static_context_path(target)?;
            path.push(static_index(index)?);
            Some(path)
        }
        _ => None,
    }
}

fn static_index(expr: &Expr) -> Option<String> {
    if let Expr::Str(value) = expr {
        return Some(value.clone());
    }
    if !is_context_free(expr) {
        return None;
    }
    let context = Context::new(Arc::new(UnreachableFs));
    // This context-free fold still occurs on the workflow-template path, so
    // it inherits the runner template's 10 MiB expression budget rather than
    // the bare expression SDK's 1 MiB default.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTObjectTemplating/ObjectTemplating/Tokens/TemplateToken.cs#L52-L65
    match evaluate_with_options(
        expr,
        &context,
        EvaluationOptions::new(WORKFLOW_TEMPLATE_MAX_MEMORY_BYTES),
    )
    .ok()?
    {
        Value::Array(_) | Value::Object(_) => None,
        primitive => Some(to_display_string(&primitive)),
    }
}

fn is_context_free(expr: &Expr) -> bool {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) => true,
        Expr::NamedValue(_) => false,
        Expr::Not(inner) | Expr::Wildcard { target: inner } => is_context_free(inner),
        Expr::Binary { lhs, rhs, .. } => is_context_free(lhs) && is_context_free(rhs),
        Expr::Index { target, index } => is_context_free(target) && is_context_free(index),
        Expr::Call { name, args } => {
            !["hashFiles", "success", "failure", "cancelled", "always"]
                .iter()
                .any(|runtime| runtime.eq_ignore_ascii_case(name))
                && args.iter().all(is_context_free)
        }
    }
}

#[derive(Debug)]
struct UnreachableFs;

impl HashFilesFs for UnreachableFs {
    fn workspace_root(&self) -> &Path {
        Path::new("")
    }

    fn open_dir(&self, _path: &Path) -> io::Result<OpenedDir<'_>> {
        Err(io::Error::other(
            "static index classification never evaluates hashFiles()",
        ))
    }

    fn entry_kind(&self, _path: &Path) -> io::Result<EntryKind> {
        Err(io::Error::other(
            "static index classification never evaluates hashFiles()",
        ))
    }

    fn hash_file_sha256(
        &self,
        _path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        Err(io::Error::other(
            "static index classification never evaluates hashFiles()",
        ))
    }
}
