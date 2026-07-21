//! The six-kind runtime value model and GitHub's exact coercion rules.
//!
//! Source for the kind set and every coercion below: the design memo's
//! "Type model and coercions" section (`Sdk/EvaluationResult.cs`,
//! `Sdk/ExpressionUtility.cs`), cross-referenced against the docs'
//! [Expressions reference](https://docs.github.com/en/actions/reference/workflows-and-actions/expressions).

use std::rc::Rc;

mod coercion;
mod comparison;

pub use coercion::{format_g15, is_falsy, is_truthy, to_display_string, to_number};
pub use comparison::{
    abstract_equal, abstract_not_equal, greater_or_equal, greater_than, less_or_equal, less_than,
    ordinal_ignore_case_cmp, ordinal_ignore_case_contains, ordinal_ignore_case_ends_with,
    ordinal_ignore_case_eq, ordinal_ignore_case_starts_with,
};

/// A GitHub Actions expression value. There are exactly six kinds
/// (`ValueKind` in the design memo, all numeric host types canonicalize to
/// [`Value::Number`] as `f64`).
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
    /// An object (insertion-ordered, case-insensitive key lookup).
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
    /// are looked up case-insensitively later (see [`ObjectValue::get`]);
    /// duplicate keys are the caller's responsibility to avoid (matching
    /// GitHub's `DictionaryContextData`, which is itself a single map — the
    /// last write for a given key wins if a caller inserts a duplicate).
    pub fn object(entries: Vec<(String, Value)>) -> Value {
        Value::Object(ObjectValue::new(entries))
    }
}

/// An array value. See the design memo's "Index / dereference evaluation"
/// section for why filtered arrays are a distinct internal flavor: indexing
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

/// An object value: insertion-ordered key/value pairs, looked up
/// case-insensitively.
///
/// Source: the design memo's "Index / dereference evaluation" section —
/// "the runner's `DictionaryContextData` uses `StringComparer.OrdinalIgnoreCase`;
/// the dictionary preserves insertion order." A linear scan is used for
/// lookup rather than a second case-folded index: context objects in
/// practice (env maps, `github.event` sub-objects, matrix entries) are small,
/// and a linear scan avoids maintaining two representations of the same
/// key that could drift.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectValue {
    entries: Rc<Vec<(String, Value)>>,
}

impl ObjectValue {
    fn new(entries: Vec<(String, Value)>) -> Self {
        ObjectValue {
            entries: Rc::new(entries),
        }
    }

    /// Case-insensitive ordinal key lookup (see the module doc comment on
    /// [`ordinal_ignore_case_eq`] for exactly what "ordinal" means here).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(k, _)| ordinal_ignore_case_eq(k, key))
            .map(|(_, v)| v)
    }

    /// Iterates entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
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
