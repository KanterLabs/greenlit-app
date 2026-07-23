//! Explicit YAML tag resolution and GitHub's scalar-typing dispatch.
//!
//! Split out of `yaml::raw` to keep that module focused on the event-driven
//! tree builder itself; this module holds the "what does this scalar
//! actually resolve to" logic the builder calls into for every
//! `Event::Scalar`/collection-start event.

use crate::error::ParseError;
use crate::span::Span;
use crate::yaml::scalar::{self, NumberParseError, YamlScalar};
use saphyr_parser::{ScalarStyle, Tag};
use std::borrow::Cow;

/// One of the four core-schema scalar tags this crate honors explicitly
/// (`!!str`, `!!bool`, `!!int`, `!!float`, `!!null`); see [`typed_scalar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoreTag {
    Str,
    Bool,
    Int,
    Float,
    Null,
}

/// Validate an explicit tag attached to a scalar, recording the first
/// violation into `error` (if none is already recorded). Collection events
/// deliberately do not call this function because the runner ignores every
/// tag attached to a sequence or mapping.
pub(crate) fn resolve_scalar_core_tag(
    tag: &Option<Cow<'_, Tag>>,
    span: &Span,
    error: &mut Option<ParseError>,
) -> Option<CoreTag> {
    let tag = tag.as_ref()?;
    if tag.handle != "tag:yaml.org,2002:" {
        error.get_or_insert(ParseError::UnsupportedTag {
            span: span.clone(),
            tag: tag.to_string(),
        });
        return None;
    }
    match tag.suffix.as_str() {
        "str" => Some(CoreTag::Str),
        "bool" => Some(CoreTag::Bool),
        "int" => Some(CoreTag::Int),
        "float" => Some(CoreTag::Float),
        "null" => Some(CoreTag::Null),
        other => {
            error.get_or_insert(ParseError::UnsupportedTag {
                span: span.clone(),
                tag: format!("!!{other}"),
            });
            None
        }
    }
}

/// Resolve one scalar's typed value, honoring the runner's tag/style
/// dispatch or, absent a tag, GitHub's plain-style matcher chain (never
/// applied to quoted/block scalars — see `crate::yaml::scalar` module docs).
pub(crate) fn typed_scalar(
    raw: &str,
    style: ScalarStyle,
    tag: Option<CoreTag>,
    span: &Span,
    error: &mut Option<ParseError>,
) -> YamlScalar {
    // The pinned runner accepts `!!str` for every scalar style, but rejects
    // every other explicit scalar tag unless the scalar is plain. Check this
    // before parsing the tag's value so quoting cannot silently override the
    // runner's style error.
    // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/WorkflowParser/Conversion/YamlObjectReader.cs#L41-L58
    if let Some(tag_kind) = tag
        && tag_kind != CoreTag::Str
        && style != ScalarStyle::Plain
    {
        error.get_or_insert(ParseError::InvalidTagStyle {
            span: span.clone(),
            style: scalar_style_name(style),
            tag_name: core_tag_name(tag_kind),
        });
        return YamlScalar::String(raw.to_owned());
    }

    match tag {
        None => match style {
            ScalarStyle::Plain => match scalar::resolve_plain(raw) {
                Ok(v) => v,
                Err(NumberParseError::RadixIntegerOverflow) => {
                    error.get_or_insert(ParseError::IntegerOverflow {
                        span: span.clone(),
                        raw: raw.to_owned(),
                    });
                    YamlScalar::String(raw.to_owned())
                }
                Err(NumberParseError::OutOfRange) => {
                    error.get_or_insert(ParseError::Schema {
                        span: span.clone(),
                        message: format!("numeric literal '{raw}' is out of range"),
                    });
                    YamlScalar::String(raw.to_owned())
                }
            },
            ScalarStyle::SingleQuoted
            | ScalarStyle::DoubleQuoted
            | ScalarStyle::Literal
            | ScalarStyle::Folded => YamlScalar::String(raw.to_owned()),
        },
        Some(CoreTag::Str) => YamlScalar::String(raw.to_owned()),
        Some(CoreTag::Null) => {
            if scalar::as_null(raw) {
                YamlScalar::Null
            } else {
                tag_mismatch(error, span, raw, "null")
            }
        }
        Some(CoreTag::Bool) => match scalar::as_bool(raw) {
            Some(b) => YamlScalar::Bool(b),
            None => tag_mismatch(error, span, raw, "bool"),
        },
        Some(CoreTag::Int) => match scalar::as_integer(raw) {
            Some(Ok(n)) => YamlScalar::Number(n),
            Some(Err(_)) | None => tag_mismatch(error, span, raw, "int"),
        },
        Some(CoreTag::Float) => match scalar::as_float(raw) {
            Some(Ok(n)) => YamlScalar::Number(n),
            Some(Err(_)) | None => tag_mismatch(error, span, raw, "float"),
        },
    }
}

fn core_tag_name(tag: CoreTag) -> &'static str {
    match tag {
        CoreTag::Str => "str",
        CoreTag::Bool => "bool",
        CoreTag::Int => "int",
        CoreTag::Float => "float",
        CoreTag::Null => "null",
    }
}

fn scalar_style_name(style: ScalarStyle) -> &'static str {
    match style {
        ScalarStyle::Plain => "Plain",
        ScalarStyle::SingleQuoted => "SingleQuoted",
        ScalarStyle::DoubleQuoted => "DoubleQuoted",
        ScalarStyle::Literal => "Literal",
        ScalarStyle::Folded => "Folded",
    }
}

fn tag_mismatch(
    error: &mut Option<ParseError>,
    span: &Span,
    raw: &str,
    tag_name: &'static str,
) -> YamlScalar {
    error.get_or_insert(ParseError::TagMismatch {
        span: span.clone(),
        raw: raw.to_owned(),
        tag_name,
    });
    YamlScalar::String(raw.to_owned())
}
