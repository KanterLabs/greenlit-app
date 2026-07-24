//! Shared helpers for walking a [`RawNode`] tree into typed manifest
//! fields.
//!
//! # String-field typing policy
//!
//! Every manifest field GitHub documents as string-typed (`name`,
//! `description`, `default`, script paths, …) is read via
//! [`scalar_string`]/[`optional_string`]/[`required_string`], which return
//! a scalar's raw decoded text regardless of whether GitHub's Null/Boolean
//! matcher chain would have typed it as something else (an unquoted
//! `default: 123` is the string `"123"`, matching the runner's own
//! permissive `ToString`-style coercion into a string-typed schema slot).
//! The one exception is an explicit YAML null (`null`/`~`/empty, unquoted)
//! — which resolves to *absent*, not the four-character string `"null"`
//! — since that is what every optional field's absence already means.
//! [`crate::manifest::ActionInput::required`] is the only genuinely
//! boolean-typed field in the schema this crate models, and is read with
//! [`scalar_bool`] instead.

use greenlit_workflow::Span;

use crate::manifest::ManifestError;
use crate::manifest::yaml::RawNode;
use greenlit_workflow::Spanned;

pub(super) type Entries = [(Spanned<RawNode>, Spanned<RawNode>)];

pub(super) fn as_mapping<'a>(
    node: &'a Spanned<RawNode>,
    context: &str,
) -> Result<&'a Entries, ManifestError> {
    match &node.value {
        RawNode::Mapping(entries) => Ok(entries),
        RawNode::Scalar(_) | RawNode::Sequence(_) => Err(ManifestError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a mapping"),
        }),
    }
}

pub(super) fn as_sequence<'a>(
    node: &'a Spanned<RawNode>,
    context: &str,
) -> Result<&'a [Spanned<RawNode>], ManifestError> {
    match &node.value {
        RawNode::Sequence(items) => Ok(items),
        RawNode::Scalar(_) | RawNode::Mapping(_) => Err(ManifestError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a sequence"),
        }),
    }
}

/// GitHub runner Null grammar (unquoted only): `""`, `null`, `Null`,
/// `NULL`, `~` — same four-way set `greenlit-workflow`'s scalar matcher
/// uses, since this is the same runner class (see `manifest::yaml` module
/// docs).
fn is_null_scalar(raw: &str) -> bool {
    matches!(raw, "" | "null" | "Null" | "NULL" | "~")
}

/// A scalar's text, or `None` if it is an explicit (unquoted) YAML null —
/// see the module docs' string-field typing policy.
pub(super) fn scalar_string(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Option<String>, ManifestError> {
    match &node.value {
        RawNode::Scalar(s) => {
            if s.is_plain() && is_null_scalar(&s.raw) {
                Ok(None)
            } else {
                Ok(Some(s.raw.clone()))
            }
        }
        RawNode::Sequence(_) | RawNode::Mapping(_) => Err(ManifestError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a scalar"),
        }),
    }
}

/// A genuinely boolean-typed field: only unquoted `true`/`True`/`TRUE`/
/// `false`/`False`/`FALSE` (GitHub's own Boolean grammar — `yes`/`no`/
/// `on`/`off` are deliberately not matched).
pub(super) fn scalar_bool(node: &Spanned<RawNode>, context: &str) -> Result<bool, ManifestError> {
    if let RawNode::Scalar(s) = &node.value
        && s.is_plain()
    {
        match s.raw.as_str() {
            "true" | "True" | "TRUE" => return Ok(true),
            "false" | "False" | "FALSE" => return Ok(false),
            _ => {}
        }
    }
    Err(ManifestError::Schema {
        span: node.span.clone(),
        message: format!("{context} must be a boolean (true/false)"),
    })
}

pub(super) fn find<'a>(entries: &'a Entries, name: &str) -> Option<&'a Spanned<RawNode>> {
    entries
        .iter()
        .find(|(k, _)| k.value.as_key_text() == Some(name))
        .map(|(_, v)| v)
}

/// [`find`] plus [`scalar_string`], returning `Ok(None)` when `name` is
/// absent from `entries` at all.
pub(super) fn optional_string(
    entries: &Entries,
    name: &str,
    context: &str,
) -> Result<Option<String>, ManifestError> {
    find(entries, name)
        .map(|node| scalar_string(node, &format!("{context}.{name}")))
        .transpose()
        .map(Option::flatten)
}

/// [`find`] plus [`scalar_string`], erroring with [`ManifestError::MissingKey`]
/// when `name` is absent *or* present but resolves to an explicit null
/// (which means the same thing here: this required field was not really
/// given a value).
pub(super) fn required_string(
    entries: &Entries,
    name: &'static str,
    span: &Span,
    context: &str,
) -> Result<String, ManifestError> {
    let node = require(entries, name, span, context)?;
    scalar_string(node, &format!("{context}.{name}"))?.ok_or_else(|| ManifestError::MissingKey {
        span: span.clone(),
        key: name,
        context: context.to_owned(),
    })
}

pub(super) fn require<'a>(
    entries: &'a Entries,
    name: &'static str,
    span: &Span,
    context: &str,
) -> Result<&'a Spanned<RawNode>, ManifestError> {
    find(entries, name).ok_or_else(|| ManifestError::MissingKey {
        span: span.clone(),
        key: name,
        context: context.to_owned(),
    })
}

pub(super) fn reject_unknown_keys(
    entries: &Entries,
    known: &[&str],
    context: &str,
) -> Result<(), ManifestError> {
    for (key, _) in entries {
        let Some(text) = key.value.as_key_text() else {
            return Err(ManifestError::NonScalarKey {
                span: key.span.clone(),
            });
        };
        if !known.contains(&text) {
            return Err(ManifestError::UnknownKey {
                span: key.span.clone(),
                key: text.to_owned(),
                context: context.to_owned(),
            });
        }
    }
    Ok(())
}
