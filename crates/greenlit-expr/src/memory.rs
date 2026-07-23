//! GitHub-compatible expression-result memory accounting.
//!
//! The runner tracks live results per expression-tree depth, trimming deeper
//! primitive results when their parent is realized. Separately, functions
//! that build amplified strings/filtered arrays count before each append.
//! Strings cost 26 bytes plus two per UTF-16 code unit, other values at least
//! 24 bytes, and a total exactly equal to the limit is accepted (`>` rejects).
//! Sources pinned for Phase 1:
//! <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/EvaluationMemory.cs>
//! <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/MemoryCounter.cs>
//! <https://github.com/actions/runner/blob/f898ef14a51cf42409469bc248492c325ad8a874/src/Sdk/DTExpressions2/Expressions2/Sdk/ExpressionNode.cs#L83-L121>

use crate::error::EvalError;
use crate::value::Value;

pub(crate) const MIN_OBJECT_BYTES: usize = 24;
pub(crate) const STRING_BASE_BYTES: usize = 26;
pub(crate) const POINTER_BYTES: usize = std::mem::size_of::<usize>();

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ResultMemory {
    pub(crate) bytes: Option<usize>,
    pub(crate) is_total: bool,
    pub(crate) has_raw: bool,
}

/// A node-local counter used while constructing amplified results.
pub(crate) struct MemoryCounter {
    current_bytes: usize,
    max_bytes: usize,
}

impl MemoryCounter {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            current_bytes: 0,
            max_bytes,
        }
    }

    pub(crate) fn add(&mut self, amount: usize) -> Result<(), EvalError> {
        let Some(next) = self.current_bytes.checked_add(amount) else {
            return Err(limit_error(self.max_bytes));
        };
        if next > self.max_bytes {
            return Err(limit_error(self.max_bytes));
        }
        self.current_bytes = next;
        Ok(())
    }

    pub(crate) fn add_string(&mut self, value: &str) -> Result<(), EvalError> {
        self.add(string_bytes(value, self.max_bytes)?)
    }

    pub(crate) fn current_bytes(&self) -> usize {
        self.current_bytes
    }
}

/// Tracks live node results by expression-tree depth.
pub(crate) struct EvaluationMemory {
    depths: Vec<usize>,
    max_active_depth: Option<usize>,
    total_bytes: usize,
    max_bytes: usize,
}

impl EvaluationMemory {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            depths: Vec::new(),
            max_active_depth: None,
            total_bytes: 0,
            max_bytes,
        }
    }

    pub(crate) fn add_result(
        &mut self,
        depth: usize,
        value: &Value,
        result_memory: ResultMemory,
    ) -> Result<(), EvalError> {
        let trim_depth = result_memory.is_total || (is_primitive(value) && !result_memory.has_raw);
        if trim_depth {
            self.trim_deeper_than(depth);
        }

        if self.depths.len() <= depth {
            self.depths.resize(depth + 1, 0);
        }
        self.max_active_depth = Some(
            self.max_active_depth
                .map_or(depth, |current| current.max(depth)),
        );

        let bytes = match result_memory.bytes {
            Some(bytes) => bytes,
            None => value_bytes(value, self.max_bytes)?,
        };
        let Some(depth_total) = self.depths[depth].checked_add(bytes) else {
            return Err(limit_error(self.max_bytes));
        };
        let Some(total) = self.total_bytes.checked_add(bytes) else {
            return Err(limit_error(self.max_bytes));
        };
        if total > self.max_bytes {
            return Err(limit_error(self.max_bytes));
        }
        self.depths[depth] = depth_total;
        self.total_bytes = total;
        Ok(())
    }

    pub(crate) fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    fn trim_deeper_than(&mut self, depth: usize) {
        let Some(mut active) = self.max_active_depth else {
            return;
        };
        while active > depth {
            let amount = self.depths.get(active).copied().unwrap_or(0);
            self.total_bytes = self.total_bytes.saturating_sub(amount);
            if let Some(slot) = self.depths.get_mut(active) {
                *slot = 0;
            }
            active -= 1;
        }
        self.max_active_depth = Some(active);
    }
}

pub(crate) fn string_bytes(value: &str, max_bytes: usize) -> Result<usize, EvalError> {
    let Some(payload) = value.encode_utf16().count().checked_mul(2) else {
        return Err(limit_error(max_bytes));
    };
    STRING_BASE_BYTES
        .checked_add(payload)
        .ok_or_else(|| limit_error(max_bytes))
}

pub(crate) fn wrapped_primitive_bytes(
    value: &Value,
    max_bytes: usize,
) -> Result<Option<usize>, EvalError> {
    if !is_primitive(value) {
        return Ok(None);
    }
    let canonical = value_bytes(value, max_bytes)?;
    MIN_OBJECT_BYTES
        .checked_add(canonical)
        .map(Some)
        .ok_or_else(|| limit_error(max_bytes))
}

fn value_bytes(value: &Value, max_bytes: usize) -> Result<usize, EvalError> {
    match value {
        Value::String(value) => string_bytes(value, max_bytes),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            Ok(MIN_OBJECT_BYTES)
        }
    }
}

fn is_primitive(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn limit_error(max_bytes: usize) -> EvalError {
    EvalError::MemoryLimitExceeded { max_bytes }
}
