//! The six-kind runtime value model and GitHub's exact coercion rules.
//!
//! The kind set and coercions follow the Actions runner's
//! [`EvaluationResult.cs`](https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/EvaluationResult.cs)
//! and
//! [`ExpressionUtility.cs`](https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/ExpressionUtility.cs),
//! cross-referenced against GitHub's
//! [Expressions reference](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions).

use std::collections::HashMap;
use std::rc::Rc;

mod coercion;
mod comparison;

pub use coercion::{format_g15, is_falsy, is_truthy, to_display_string, to_number};
pub use comparison::{
    abstract_equal, abstract_not_equal, greater_or_equal, greater_than, less_or_equal, less_than,
    ordinal_ignore_case_cmp, ordinal_ignore_case_contains, ordinal_ignore_case_ends_with,
    ordinal_ignore_case_eq, ordinal_ignore_case_key, ordinal_ignore_case_starts_with,
};

/// A GitHub Actions expression value. There are exactly six runner value
/// kinds; all numeric host types canonicalize to [`Value::Number`] as `f64`.
///
/// `Array`/`Object` hold an `Rc` so that `==`/`!=` can implement GitHub's
/// documented "same instance" reference-identity rule for collections (see
/// [`abstract_equal`]) — consequently `Value` is `Clone` but not `Send`;
/// evaluation is expected to happen on a single thread per expression, which
/// matches how `greenlit-engine` calls into this crate (there is no
/// concurrent mutation of a value once built).
///
/// The derived [`PartialEq`] is ordinary Rust structural equality (deep
/// value comparison, including comparing `Array`/`Object` contents rather
/// than instance identity) — a convenience for assertions in this crate's
/// own tests and for callers who want a plain equality check. It is a
/// *different* operation from GitHub's own loose `==` semantics, which are
/// always the explicit [`abstract_equal`] function (never Rust's `==`
/// operator), because GitHub's coercion/identity rules would be a
/// surprising override of `PartialEq`'s normal meaning.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// The `null` value.
    Null,
    /// A `true`/`false` boolean.
    Bool(bool),
    /// A number — GitHub Actions has exactly one numeric kind, an IEEE-754
    /// `f64` (no separate integer type).
    Number(f64),
    /// A string.
    String(String),
    /// An array (see [`ArrayValue`] for the plain-vs-filtered distinction).
    Array(ArrayValue),
    /// An object (insertion-ordered, with an explicit key-comparison policy).
    Object(ObjectValue),
}

/// The kind tag of a [`Value`], used throughout the coercion rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// [`Value::Null`]'s kind.
    Null,
    /// [`Value::Bool`]'s kind.
    Boolean,
    /// [`Value::Number`]'s kind.
    Number,
    /// [`Value::String`]'s kind.
    String,
    /// [`Value::Array`]'s kind.
    Array,
    /// [`Value::Object`]'s kind.
    Object,
}

impl Value {
    /// Returns this value's [`ValueKind`].
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Null => ValueKind::Null,
            Value::Bool(_) => ValueKind::Boolean,
            Value::Number(_) => ValueKind::Number,
            Value::String(_) => ValueKind::String,
            Value::Array(_) => ValueKind::Array,
            Value::Object(_) => ValueKind::Object,
        }
    }

    /// Builds an ordinary (non-filtered) array value from owned items.
    pub fn array(items: Vec<Value>) -> Value {
        Value::Array(ArrayValue::new(items, false))
    }

    /// Builds a filtered-array value (the result of an object-filter `*`
    /// expression) from owned items. See [`ArrayValue::is_filtered`] for why
    /// this is a distinct flavor.
    pub fn filtered_array(items: Vec<Value>) -> Value {
        Value::Array(ArrayValue::new(items, true))
    }

    /// Builds an object value from owned, insertion-ordered entries. Keys
    /// are looked up case-insensitively later (see [`ObjectValue::get`]).
    /// Case-insensitive duplicates collapse in place: the first spelling
    /// and position are preserved while the last value wins, matching
    /// GitHub's `DictionaryContextData` string indexer.
    pub fn object(entries: Vec<(String, Value)>) -> Value {
        Value::Object(ObjectValue::new(entries, false))
    }

    /// Builds an insertion-ordered object with case-sensitive key lookup.
    ///
    /// Most Actions context objects use ordinal-ignore-case keys; the Linux
    /// `env` context is the important exception. [`crate::Context::with_env`]
    /// applies this policy automatically, so callers normally need this
    /// constructor only for a custom context implementation.
    pub fn case_sensitive_object(entries: Vec<(String, Value)>) -> Value {
        Value::Object(ObjectValue::new(entries, true))
    }
}

/// An array value. The runner's
/// [`Index.cs`](https://github.com/actions/runner/blob/main/src/Sdk/DTExpressions2/Expressions2/Sdk/Operators/Index.cs)
/// makes filtered arrays a distinct internal flavor: indexing
/// a *filtered* array by string key maps the lookup over each element
/// (silently skipping elements that don't have it), whereas indexing an
/// *ordinary* array by string key converts the key with `ToNumber` (almost
/// always `NaN` for a real property name) and returns `Null`. Both flavors
/// are otherwise ordinary arrays for the rest of the type system (`join`,
/// `contains`, `toJSON`, truthiness).
#[derive(Debug, Clone, PartialEq)]
pub struct ArrayValue {
    items: Rc<Vec<Value>>,
    filtered: bool,
}

impl ArrayValue {
    fn new(items: Vec<Value>, filtered: bool) -> Self {
        ArrayValue {
            items: Rc::new(items),
            filtered,
        }
    }

    /// The array's elements, in order.
    pub fn items(&self) -> &[Value] {
        &self.items
    }

    /// The number of elements.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the array has no elements.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether this array is the result of an object-filter `*` expression.
    pub fn is_filtered(&self) -> bool {
        self.filtered
    }

    /// Reference-identity check for `==`/`!=` (see [`abstract_equal`]):
    /// "Objects and arrays are only considered equal when they are the same
    /// instance."
    fn same_instance(&self, other: &ArrayValue) -> bool {
        Rc::ptr_eq(&self.items, &other.items)
    }
}

/// An object value: insertion-ordered key/value pairs with either exact or
/// ordinal-ignore-case lookup, selected when the object is constructed.
///
/// The runner's ordinary `DictionaryContextData` uses
/// `StringComparer.OrdinalIgnoreCase`, while its Linux `env` context uses
/// `CaseSensitiveDictionaryContextData`; both retain insertion order:
/// <https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ContextData/DictionaryContextData.cs>.
/// <https://github.com/actions/runner/blob/main/src/Runner.Worker/ExecutionContext.cs>.
/// A canonical-key index makes lookup constant-time while the ordered entry
/// vector remains the source of truth for iteration and serialization.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectValue {
    entries: Rc<Vec<(String, Value)>>,
    positions: Rc<HashMap<String, usize>>,
    case_sensitive: bool,
}

impl ObjectValue {
    fn new(entries: Vec<(String, Value)>, case_sensitive: bool) -> Self {
        // `DictionaryContextData`'s string indexer replaces an existing
        // value under its configured comparer while preserving the first
        // key's spelling and position. This matters for values such as
        // `fromJSON('{"A": 1, "a": 2}')`: ordinary expression objects
        // compare keys ordinal-ignore-case, so `a` replaces `A` rather than
        // creating a second property.
        // https://github.com/actions/runner/blob/main/src/Sdk/DTPipelines/Pipelines/ContextData/DictionaryContextData.cs
        let mut normalized: Vec<(String, Value)> = Vec::with_capacity(entries.len());
        let mut positions: HashMap<String, usize> = HashMap::with_capacity(entries.len());
        for (key, value) in entries {
            let lookup_key = if case_sensitive {
                key.clone()
            } else {
                comparison::ordinal_ignore_case_key(&key)
            };
            match positions.entry(lookup_key) {
                std::collections::hash_map::Entry::Occupied(mut occupied) => {
                    if let Some((_, stored_value)) = normalized.get_mut(*occupied.get()) {
                        *stored_value = value;
                    } else {
                        // Keep construction total even if the private index
                        // invariant is changed later: repair the index and
                        // retain the entry instead of indexing with a panic.
                        *occupied.get_mut() = normalized.len();
                        normalized.push((key, value));
                    }
                }
                std::collections::hash_map::Entry::Vacant(vacant) => {
                    vacant.insert(normalized.len());
                    normalized.push((key, value));
                }
            }
        }
        ObjectValue {
            entries: Rc::new(normalized),
            positions: Rc::new(positions),
            case_sensitive,
        }
    }

    /// Looks up a key using this object's exact or ordinal-ignore-case
    /// comparison policy.
    pub fn get(&self, key: &str) -> Option<&Value> {
        let lookup_key = if self.case_sensitive {
            key.to_string()
        } else {
            comparison::ordinal_ignore_case_key(key)
        };
        self.positions
            .get(&lookup_key)
            .and_then(|index| self.entries.get(*index))
            .map(|(_, value)| value)
    }

    /// Iterates entries in insertion order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&str, &Value)> + ExactSizeIterator {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the object has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn same_instance(&self, other: &ObjectValue) -> bool {
        Rc::ptr_eq(&self.entries, &other.entries)
    }
}
