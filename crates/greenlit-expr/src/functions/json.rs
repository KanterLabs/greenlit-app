//! `toJSON(value)` / `fromJSON(value)` as documented at
//! <https://docs.github.com/en/actions/reference/workflows-and-actions/expressions#tojson>.

use crate::error::EvalError;
use crate::memory::MemoryCounter;
use crate::value::{Value, format_g15};

mod from_json;

pub use from_json::FromJsonError;
pub(crate) use from_json::from_json;

// ---------------------------------------------------------------------
// toJSON
// ---------------------------------------------------------------------

/// `toJSON(value)`. Rendering fails if it crosses the same result-memory
/// limit as the runner. The runner's implementation is
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/ToJson.cs>.
/// Numbers render
/// bare/unquoted using the same `G15` formatter as `ToString` (so
/// `toJSON(fromJSON('1e400'))` deliberately emits the bare, invalid-JSON
/// token `Infinity`, reproducing GitHub's behavior rather than "fixing" it),
/// 2-space-per-level indentation, empty array/object render inline as
/// `[]`/`{}`, and object keys use Newtonsoft-style JSON string escaping.
pub(crate) fn to_json(value: &Value, max_memory_bytes: usize) -> Result<Value, EvalError> {
    let mut writer = JsonWriter {
        out: String::new(),
        memory: MemoryCounter::new(max_memory_bytes),
    };
    writer.write(value)?;
    Ok(Value::String(writer.out))
}

#[derive(Clone, Copy)]
enum Parent {
    Array { first: bool },
    Object,
}

struct JsonWriter {
    out: String,
    memory: MemoryCounter,
}

enum WriteTask<'a> {
    Value {
        value: &'a Value,
        depth: usize,
        parent: Option<Parent>,
    },
    MappingKey {
        key: &'a str,
        depth: usize,
        first: bool,
    },
    CollectionEnd {
        closing: char,
        depth: usize,
    },
}

impl JsonWriter {
    fn write(&mut self, value: &Value) -> Result<(), EvalError> {
        // ToJson.cs uses an explicit ancestor stack. Keeping this traversal
        // iterative means a public, hand-built deeply nested Value cannot
        // overflow the Rust call stack before the memory budget rejects it.
        // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/ToJson.cs#L16-L155
        let mut tasks = vec![WriteTask::Value {
            value,
            depth: 0,
            parent: None,
        }];
        while let Some(task) = tasks.pop() {
            match task {
                WriteTask::Value {
                    value,
                    depth,
                    parent,
                } => match value {
                    Value::Array(array) if !array.is_empty() => {
                        self.append_prefixed("[", depth, parent)?;
                        tasks.push(WriteTask::CollectionEnd {
                            closing: ']',
                            depth,
                        });
                        for (index, item) in array.items().iter().enumerate().rev() {
                            tasks.push(WriteTask::Value {
                                value: item,
                                depth: depth + 1,
                                parent: Some(Parent::Array { first: index == 0 }),
                            });
                        }
                    }
                    Value::Object(object) if !object.is_empty() => {
                        self.append_prefixed("{", depth, parent)?;
                        tasks.push(WriteTask::CollectionEnd {
                            closing: '}',
                            depth,
                        });
                        for (index, (key, item)) in object.iter().enumerate().rev() {
                            tasks.push(WriteTask::Value {
                                value: item,
                                depth: depth + 1,
                                parent: Some(Parent::Object),
                            });
                            tasks.push(WriteTask::MappingKey {
                                key,
                                depth: depth + 1,
                                first: index == 0,
                            });
                        }
                    }
                    other => self.write_primitive_or_empty(other, depth, parent)?,
                },
                WriteTask::MappingKey { key, depth, first } => {
                    let mut rendered = String::new();
                    write_json_string(key, &mut rendered);
                    self.append_sequence_value(&rendered, depth, first)?;
                }
                WriteTask::CollectionEnd { closing, depth } => {
                    self.append_raw(&format!("\n{}{closing}", " ".repeat(depth * 2)))?;
                }
            }
        }
        Ok(())
    }

    fn write_primitive_or_empty(
        &mut self,
        value: &Value,
        depth: usize,
        parent: Option<Parent>,
    ) -> Result<(), EvalError> {
        let mut rendered = String::new();
        match value {
            Value::Null => rendered.push_str("null"),
            Value::Bool(value) => {
                rendered.push_str(if *value { "true" } else { "false" });
            }
            // Bare/unquoted G15, including NaN/Infinity, matches ToJson.cs
            // even though those are not JSON tokens.
            Value::Number(value) => rendered.push_str(&format_g15(*value)),
            Value::String(value) => write_json_string(value, &mut rendered),
            Value::Array(_) => rendered.push_str("[]"),
            Value::Object(_) => rendered.push_str("{}"),
        }
        self.append_prefixed(&rendered, depth, parent)
    }

    fn append_prefixed(
        &mut self,
        value: &str,
        depth: usize,
        parent: Option<Parent>,
    ) -> Result<(), EvalError> {
        match parent {
            Some(Parent::Object) => self.append_raw(&format!(": {value}")),
            Some(Parent::Array { first }) => self.append_sequence_value(value, depth, first),
            None => self.append_raw(value),
        }
    }

    fn append_sequence_value(
        &mut self,
        value: &str,
        depth: usize,
        first: bool,
    ) -> Result<(), EvalError> {
        let comma = if first { "" } else { "," };
        self.append_raw(&format!("{comma}\n{}{value}", " ".repeat(depth * 2)))
    }

    fn append_raw(&mut self, segment: &str) -> Result<(), EvalError> {
        // ToJson.cs accounts each PrefixValue/end fragment as its own .NET
        // string and rejects only when the cumulative total is greater than
        // MaxMemory.
        // https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/Functions/ToJson.cs#L158-L264
        self.memory.add_string(segment)?;
        self.out.push_str(segment);
        Ok(())
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
