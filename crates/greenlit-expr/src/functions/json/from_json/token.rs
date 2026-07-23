//! Private JToken-like values used before runner `PipelineContextData`
//! conversion.

use std::collections::HashMap;

use crate::value::Value;

/// A parsed number keeps both the runner-visible `Double` and the textual
/// representation Json.NET would emit if an enclosing `JConstructor` is
/// converted through `JToken.ToString()`.
pub(super) struct ParsedNumber {
    pub(super) value: f64,
    pub(super) rendering: String,
}

pub(super) enum ParsedValue {
    Null,
    Bool(bool),
    Number(ParsedNumber),
    String(String),
    Array(Vec<ParsedValue>),
    Object(Vec<(String, ParsedValue)>),
    Undefined,
    Constructor {
        name: String,
        arguments: Vec<ParsedValue>,
    },
}

impl ParsedValue {
    pub(super) fn object(authored: Vec<(String, ParsedValue)>) -> Self {
        // JObject's default DuplicatePropertyNameHandling is Replace and is
        // ordinal case-sensitive. The later PipelineContextData dictionary
        // conversion performs the runner's separate ignore-case collapse.
        let mut entries: Vec<(String, ParsedValue)> = Vec::with_capacity(authored.len());
        let mut positions: HashMap<String, usize> = HashMap::with_capacity(authored.len());
        for (key, value) in authored {
            if let Some(index) = positions.get(&key).copied() {
                entries[index].1 = value;
            } else {
                positions.insert(key.clone(), entries.len());
                entries.push((key, value));
            }
        }
        Self::Object(entries)
    }

    pub(super) fn into_expression_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(number) => Value::Number(number.value),
            Self::String(value) => Value::String(value),
            Self::Array(items) => {
                Value::array(items.into_iter().map(Self::into_expression_value).collect())
            }
            Self::Object(entries) => Value::object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, value.into_expression_value()))
                    .collect(),
            ),
            // JTokenExtensions.ToPipelineContextData stringifies JToken kinds
            // it does not explicitly understand. Undefined has a null
            // payload, so JValue.ToString() is empty; constructors render as
            // JSON.NET's indented JavaScript constructor syntax.
            // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTPipelines/Pipelines/ContextData/JTokenExtensions.cs
            Self::Undefined => Value::String(String::new()),
            constructor @ Self::Constructor { .. } => {
                Value::String(constructor.render_newtonsoft())
            }
        }
    }

    fn render_newtonsoft(&self) -> String {
        let mut rendered = String::new();
        self.render_at_depth(0, &mut rendered);
        rendered
    }

    fn render_at_depth(&self, depth: usize, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Number(number) => out.push_str(&number.rendering),
            Self::String(value) => super::super::write_json_string(value, out),
            Self::Undefined => out.push_str("undefined"),
            Self::Array(items) => render_sequence('[', ']', items, depth, out),
            Self::Object(entries) => {
                if entries.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (index, (key, value)) in entries.iter().enumerate() {
                    indent(depth + 1, out);
                    super::super::write_json_string(key, out);
                    out.push_str(": ");
                    value.render_at_depth(depth + 1, out);
                    if index + 1 != entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                indent(depth, out);
                out.push('}');
            }
            Self::Constructor { name, arguments } => {
                out.push_str("new ");
                out.push_str(name);
                out.push_str("(\n");
                for (index, value) in arguments.iter().enumerate() {
                    indent(depth + 1, out);
                    value.render_at_depth(depth + 1, out);
                    if index + 1 != arguments.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                indent(depth, out);
                out.push(')');
            }
        }
    }
}

fn render_sequence(
    opening: char,
    closing: char,
    items: &[ParsedValue],
    depth: usize,
    out: &mut String,
) {
    if items.is_empty() {
        out.push(opening);
        out.push(closing);
        return;
    }
    out.push(opening);
    out.push('\n');
    for (index, value) in items.iter().enumerate() {
        indent(depth + 1, out);
        value.render_at_depth(depth + 1, out);
        if index + 1 != items.len() {
            out.push(',');
        }
        out.push('\n');
    }
    indent(depth, out);
    out.push(closing);
}

fn indent(depth: usize, out: &mut String) {
    for _ in 0..depth * 2 {
        out.push(' ');
    }
}
