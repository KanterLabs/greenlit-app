//! `toJSON(value)` / `fromJSON(value)` — design memo §3.5 and §3.6.

use crate::value::{Value, format_g15};

mod from_json;

pub use from_json::FromJsonError;
pub(crate) use from_json::from_json;

// ---------------------------------------------------------------------
// toJSON
// ---------------------------------------------------------------------

/// `toJSON(value)`. Never errors. Source: design memo §3.5 — numbers render
/// bare/unquoted using the same `G15` formatter as `ToString` (so
/// `toJSON(fromJSON('1e400'))` deliberately emits the bare, invalid-JSON
/// token `Infinity`, reproducing GitHub's behavior rather than "fixing" it),
/// 2-space-per-level indentation, empty array/object render inline as
/// `[]`/`{}`, and object keys use Newtonsoft-style JSON string escaping.
pub(crate) fn to_json(value: &Value) -> Value {
    let mut out = String::new();
    write_json(value, 0, &mut out);
    Value::String(out)
}

fn write_json(value: &Value, depth: usize, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        // Bare/unquoted, G15-formatted — including NaN/Infinity, which are
        // not valid JSON tokens. This is deliberate: see the doc comment
        // above.
        Value::Number(n) => out.push_str(&format_g15(*n)),
        Value::String(s) => write_json_string(s, out),
        Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            out.push('\n');
            let inner_indent = "  ".repeat(depth + 1);
            let len = arr.len();
            for (idx, item) in arr.items().iter().enumerate() {
                out.push_str(&inner_indent);
                write_json(item, depth + 1, out);
                if idx + 1 < len {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(depth));
            out.push(']');
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            out.push('\n');
            let inner_indent = "  ".repeat(depth + 1);
            let len = obj.len();
            for (idx, (key, item)) in obj.iter().enumerate() {
                out.push_str(&inner_indent);
                write_json_string(key, out);
                out.push_str(": ");
                write_json(item, depth + 1, out);
                if idx + 1 < len {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(depth));
            out.push('}');
        }
    }
}

/// Newtonsoft `JsonConvert.ToString`-style escaping: `\" \\ \b \f \n \r \t`,
/// other control characters as `\uXXXX`.
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
