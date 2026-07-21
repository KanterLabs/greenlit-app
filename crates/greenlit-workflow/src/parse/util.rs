//! Shared helpers for walking a [`RawNode`] tree into the typed model.
//!
//! ## Unknown-key policy
//!
//! Every mapping-shaped construct (the workflow root, a job, a step, …) is
//! validated against a fixed list of the keys this crate models. Any other
//! key is a hard [`ParseError::UnknownKey`] — including real GitHub keys
//! this phase simply does not model (see the crate-level docs' "Known
//! limitations" for the specific list, e.g. job-level `permissions:`,
//! `run-name:`). This mirrors GitHub's own strict schema validation more
//! closely than silently ignoring unrecognized keys would, and keeps the
//! model's shape exactly what `PHASE-1-engine-core.md` specifies rather
//! than silently accepting-and-dropping fields nobody asked for.

use crate::error::ParseError;
use crate::model::value::{ScalarOrExpr, YamlScalar, YamlValue};
use crate::model::workflow::{Defaults, RunDefaults};
use crate::span::Spanned;
use crate::yaml::raw::{RawNode, RawScalar};

pub(crate) type Entries = [(Spanned<RawNode>, Spanned<RawNode>)];

/// A mapping of scalar-or-expression values by name (`env:`, `with:`, and
/// similar name-to-value mappings share this shape).
pub(crate) type ScalarOrExprMap = Vec<(Spanned<String>, Spanned<ScalarOrExpr>)>;

/// Borrow `node` as a mapping's entry list, or a [`ParseError::Schema`]
/// naming `context`.
pub(crate) fn as_mapping<'a>(
    node: &'a Spanned<RawNode>,
    context: &str,
) -> Result<&'a Entries, ParseError> {
    match &node.value {
        RawNode::Mapping(entries) => Ok(entries),
        RawNode::Scalar(_) | RawNode::Sequence(_) => Err(ParseError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a mapping"),
        }),
    }
}

/// Borrow `node` as a sequence's item list, or a [`ParseError::Schema`]
/// naming `context`.
pub(crate) fn as_sequence<'a>(
    node: &'a Spanned<RawNode>,
    context: &str,
) -> Result<&'a [Spanned<RawNode>], ParseError> {
    match &node.value {
        RawNode::Sequence(items) => Ok(items),
        RawNode::Scalar(_) | RawNode::Mapping(_) => Err(ParseError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a sequence"),
        }),
    }
}

/// Borrow `node` as a scalar, or a [`ParseError::Schema`] naming `context`.
pub(crate) fn as_scalar<'a>(
    node: &'a Spanned<RawNode>,
    context: &str,
) -> Result<&'a RawScalar, ParseError> {
    match &node.value {
        RawNode::Scalar(s) => Ok(s),
        RawNode::Sequence(_) | RawNode::Mapping(_) => Err(ParseError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a scalar"),
        }),
    }
}

/// A mapping key's raw text. Mapping keys are guaranteed to be scalars by
/// the time `crate::parse` ever sees the tree — `crate::yaml::raw`'s
/// builder already rejects non-scalar keys before `parse_raw` returns `Ok`
/// — so this only exists to avoid a second, panicking assumption at this
/// layer; the error path is unreachable in practice.
pub(crate) fn key_text(node: &Spanned<RawNode>) -> Result<&str, ParseError> {
    node.value.as_key_text().ok_or_else(|| ParseError::Schema {
        span: node.span.clone(),
        message: "mapping keys must be scalars".to_owned(),
    })
}

/// Find `name`'s value in a mapping's entries, if present.
pub(crate) fn find<'a>(entries: &'a Entries, name: &str) -> Option<&'a Spanned<RawNode>> {
    entries
        .iter()
        .find(|(k, _)| k.value.as_key_text() == Some(name))
        .map(|(_, v)| v)
}

/// Find `name`'s key/value pair in a mapping's entries, if present (for
/// callers that need the *key's* span, e.g. to locate an
/// [`crate::model::value::UnsupportedConstruct`]).
pub(crate) fn find_pair<'a>(
    entries: &'a Entries,
    name: &str,
) -> Option<(&'a Spanned<RawNode>, &'a Spanned<RawNode>)> {
    entries
        .iter()
        .find(|(k, _)| k.value.as_key_text() == Some(name))
        .map(|(k, v)| (k, v))
}

/// Error if any key in `entries` is not in `known`.
pub(crate) fn reject_unknown_keys(
    entries: &Entries,
    known: &[&str],
    context: &str,
) -> Result<(), ParseError> {
    for (key, _) in entries {
        let text = key_text(key)?;
        if !known.contains(&text) {
            return Err(ParseError::UnknownKey {
                span: key.span.clone(),
                key: text.to_owned(),
                context: context.to_owned(),
            });
        }
    }
    Ok(())
}

/// Error if `name` is absent from `entries`.
pub(crate) fn require<'a>(
    entries: &'a Entries,
    name: &'static str,
    span: &crate::span::Span,
    context: &str,
) -> Result<&'a Spanned<RawNode>, ParseError> {
    find(entries, name).ok_or_else(|| ParseError::MissingKey {
        span: span.clone(),
        key: name,
        context: context.to_owned(),
    })
}

/// A scalar node classified as literal-or-expression (see
/// [`ScalarOrExpr::classify`]).
pub(crate) fn scalar_or_expr(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<ScalarOrExpr, ParseError> {
    let s = as_scalar(node, context)?;
    Ok(ScalarOrExpr::classify(&s.raw, || s.value.clone()))
}

/// [`scalar_or_expr`], paired with `node`'s span.
pub(crate) fn spanned_scalar_or_expr(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Spanned<ScalarOrExpr>, ParseError> {
    Ok(Spanned::new(
        scalar_or_expr(node, context)?,
        node.span.clone(),
    ))
}

/// Require `node` to be a scalar that resolved to GitHub's Boolean kind
/// (literal `true`/`false` only — see `crate::yaml::scalar`). Used for
/// schema-level boolean fields that GitHub never evaluates as expressions
/// (e.g. `workflow_dispatch.inputs.<name>.required`), unlike the
/// [`ScalarOrExpr`]-typed run-time boolean fields (`continue-on-error`, …).
pub(crate) fn expect_bool(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Spanned<bool>, ParseError> {
    let s = as_scalar(node, context)?;
    match s.value {
        YamlScalar::Bool(b) => Ok(Spanned::new(b, node.span.clone())),
        _ => Err(ParseError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a boolean (true or false)"),
        }),
    }
}

/// Require `node` to be a YAML string rather than another scalar kind.
pub(crate) fn expect_string(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Spanned<String>, ParseError> {
    let scalar = as_scalar(node, context)?;
    if matches!(scalar.value, YamlScalar::String(_)) {
        Ok(Spanned::new(scalar.raw.clone(), node.span.clone()))
    } else {
        Err(ParseError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a string"),
        })
    }
}

/// Require `node` to be a sequence containing only YAML strings.
pub(crate) fn expect_string_sequence(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Vec<Spanned<String>>, ParseError> {
    let items = as_sequence(node, context)?;
    items
        .iter()
        .map(|item| expect_string(item, context))
        .collect()
}

/// A scalar node's raw text, verbatim (ignoring GitHub's scalar typing —
/// for identifier-like positions: job/step ids, env var names, event
/// names, `uses:`/`run:` text).
pub(crate) fn raw_string(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Spanned<String>, ParseError> {
    let s = as_scalar(node, context)?;
    Ok(Spanned::new(s.raw.clone(), node.span.clone()))
}

/// A scalar-or-sequence-of-scalars node, normalized to a list of raw
/// strings (used for `needs:`, `branches:`, `types:`, …).
pub(crate) fn string_list(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Vec<Spanned<String>>, ParseError> {
    match &node.value {
        RawNode::Scalar(s) => Ok(vec![Spanned::new(s.raw.clone(), node.span.clone())]),
        RawNode::Sequence(items) => items.iter().map(|item| raw_string(item, context)).collect(),
        RawNode::Mapping(_) => Err(ParseError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a string or a list of strings"),
        }),
    }
}

/// A mapping of scalar-or-expression values (used for every `env:`,
/// `with:`, and similar name-to-value mapping in the model).
pub(crate) fn scalar_or_expr_map(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<ScalarOrExprMap, ParseError> {
    let entries = as_mapping(node, context)?;
    entries
        .iter()
        .map(|(k, v)| {
            let name = Spanned::new(key_text(k)?.to_owned(), k.span.clone());
            Ok((name, spanned_scalar_or_expr(v, context)?))
        })
        .collect()
}

/// A sequence of scalar-or-expression values (used for `ports:`/`volumes:`).
pub(crate) fn scalar_or_expr_seq(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Vec<Spanned<ScalarOrExpr>>, ParseError> {
    let items = as_sequence(node, context)?;
    items
        .iter()
        .map(|item| spanned_scalar_or_expr(item, context))
        .collect()
}

const DEFAULTS_KEYS: &[&str] = &["run"];
const RUN_DEFAULTS_KEYS: &[&str] = &["shell", "working-directory"];

/// Parse a `defaults:` mapping — identical shape at the workflow and job
/// levels, so shared between `crate::parse::workflow` and
/// `crate::parse::job`.
pub(crate) fn parse_defaults(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Defaults, ParseError> {
    let entries = as_mapping(node, context)?;
    reject_unknown_keys(entries, DEFAULTS_KEYS, context)?;
    let run = match find(entries, "run") {
        Some(v) => {
            let run_entries = as_mapping(v, context)?;
            reject_unknown_keys(run_entries, RUN_DEFAULTS_KEYS, context)?;
            let shell = find(run_entries, "shell")
                .map(|s| raw_string(s, context))
                .transpose()?;
            let working_directory = find(run_entries, "working-directory")
                .map(|w| spanned_scalar_or_expr(w, context))
                .transpose()?;
            Some(Spanned::new(
                RunDefaults {
                    shell,
                    working_directory,
                },
                v.span.clone(),
            ))
        }
        None => None,
    };
    Ok(Defaults { run })
}

/// Convert any raw node into the generic recursive [`YamlValue`] — used for
/// matrix axis values, `include`/`exclude` entries, and other genuinely
/// free-form positions.
pub(crate) fn to_yaml_value(node: &Spanned<RawNode>) -> YamlValue {
    match &node.value {
        RawNode::Scalar(s) => YamlValue::Scalar(ScalarOrExpr::classify(&s.raw, || s.value.clone())),
        RawNode::Sequence(items) => YamlValue::Sequence(
            items
                .iter()
                .map(|item| Spanned::new(to_yaml_value(item), item.span.clone()))
                .collect(),
        ),
        RawNode::Mapping(entries) => {
            let mut out = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                // Guaranteed scalar by the raw-tree invariant (see
                // `key_text` docs); an empty-string fallback is harmless
                // and keeps this conversion infallible.
                let text = k.value.as_key_text().unwrap_or_default().to_owned();
                out.push((
                    Spanned::new(text, k.span.clone()),
                    Spanned::new(to_yaml_value(v), v.span.clone()),
                ));
            }
            YamlValue::Mapping(out)
        }
    }
}
