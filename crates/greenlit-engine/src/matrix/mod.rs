//! `strategy.matrix`/`include`/`exclude`/`fail-fast`/`max-parallel` →
//! [`StrategyPlan`]: the four-phase matrix expansion algorithm.
//!
//! Source: design memo §1 ("Matrix expansion"), transcribing
//! [Running variations of jobs in a workflow](https://docs.github.com/en/actions/writing-workflows/choosing-what-your-workflow-does/running-variations-of-jobs-in-a-workflow)
//! and the `strategy`/`matrix` rows of the
//! [workflow syntax reference](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax).
//! The core cartesian-product/exclude/include algorithm and its four worked
//! examples (A: product ordering, B: include precedence/standalone-
//! additions, C: exclude partial-match, D: exclude-then-include reordering,
//! all transcribed verbatim from the design memo, which itself transcribes
//! GitHub's own documented examples) live in the `expand` submodule — get
//! those four exactly right and every real-world matrix workflow expands
//! correctly.

mod expand;

use std::num::NonZeroU32;

use indexmap::IndexMap;

use greenlit_workflow::Span;
use greenlit_workflow::model::job::Strategy;
use greenlit_workflow::model::value::ScalarOrExpr;

use crate::defer::DeferReason;
use crate::lints::Lint;
use crate::partial_eval::{FoldCtx, Folded, PartialEvalError, fold_scalar_or_expr};

/// Default (and v0-only) cap on how many legs one matrix may expand to —
/// GitHub's own documented limit (design memo §1.2 Phase 4).
pub const DEFAULT_MAX_MATRIX_LEGS: usize = 256;

/// `strategy:`, plan-time resolved.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StrategyPlan {
    /// Whether the job declared `strategy.matrix`. This remains `true`
    /// when `exclude` removes every combination, distinguishing a
    /// zero-instance matrix job from a non-matrix job's one implicit
    /// instance.
    pub is_matrix: bool,
    /// Default `true`. Runtime cancellation semantics are the execution
    /// phase's job; the plan only carries the declared value.
    pub fail_fast: bool,
    /// `None` = unlimited (bounded only by executor capacity).
    pub max_parallel: Option<NonZeroU32>,
    /// Expanded combinations. This can be empty either because
    /// [`StrategyPlan::is_matrix`] is `false`, or because every declared
    /// matrix combination was excluded.
    pub legs: Vec<MatrixLeg>,
}

/// One expanded matrix combination.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MatrixLeg {
    /// Stable ordinal within the job (final leg list order).
    pub index: usize,
    /// This leg's `axis: value` pairs, in key declaration order.
    pub values: IndexMap<String, MatrixValue>,
    /// Where this leg came from.
    pub origin: LegOrigin,
}

impl MatrixLeg {
    /// GHA-style display name: `job-name (v1, v2, …)` joining the leg's
    /// values in key order (design memo §1.2 Phase 4, "Leg identity").
    #[must_use]
    pub fn display_suffix(&self) -> String {
        if self.values.is_empty() {
            return String::new();
        }
        let joined = self
            .values
            .values()
            .map(MatrixValue::display)
            .collect::<Vec<_>>()
            .join(", ");
        format!(" ({joined})")
    }
}

/// Where a [`MatrixLeg`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum LegOrigin {
    /// A surviving cartesian-product combination.
    Product,
    /// Created because `include` entry `entry_index` (0-based, list order)
    /// did not fit any surviving product combination.
    Include {
        /// The include entry's index.
        entry_index: usize,
    },
}

/// A deep-equality-comparable mirror of a matrix axis/`include`/`exclude`
/// value — GitHub permits object values here (design memo §1.1), addressed
/// as `matrix.key.subkey`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum MatrixValue {
    /// `null`.
    Null,
    /// `true`/`false`.
    Bool(bool),
    /// A number.
    Number(f64),
    /// A string.
    String(String),
    /// A sequence.
    Sequence(Vec<MatrixValue>),
    /// A mapping, insertion-ordered.
    Mapping(IndexMap<String, MatrixValue>),
}

impl MatrixValue {
    /// Renders a value for [`MatrixLeg::display_suffix`] — a short,
    /// human-readable token, not a data-preserving serialization.
    fn display(&self) -> String {
        match self {
            MatrixValue::Null => "null".to_string(),
            MatrixValue::Bool(b) => b.to_string(),
            MatrixValue::Number(n) => greenlit_expr::value::format_g15(*n),
            MatrixValue::String(s) => s.clone(),
            MatrixValue::Sequence(_) => "[…]".to_string(),
            MatrixValue::Mapping(_) => "{…}".to_string(),
        }
    }
}

/// Everything that can go wrong expanding a `strategy:` block.
#[derive(Debug, thiserror::Error)]
pub enum MatrixError {
    /// `exclude:` had an entry with no keys at all — this would vacuously
    /// match (and remove) every combination, which is never what a user
    /// means (design memo §1.2 Phase 2).
    #[error("{span}: an `exclude` entry must name at least one matrix key")]
    EmptyExcludeEntry {
        /// The empty entry's span.
        span: Span,
    },
    /// The expanded leg count exceeds the cap.
    #[error("{span}: matrix expands to {count} jobs, exceeding the limit of {cap}")]
    TooManyLegs {
        /// How many legs were produced.
        count: usize,
        /// The configured cap.
        cap: usize,
        /// The `strategy.matrix` span.
        span: Span,
    },
    /// A matrix axis/`include`/`exclude` value contained an expression that
    /// referenced runtime-only data — a matrix must be fully known before
    /// any job starts, so this is a hard v0 error, not a deferral.
    #[error(
        "{span}: matrix value depends on data not available at plan time (needs {defers_on:?})"
    )]
    ValueNotStatic {
        /// Where the offending value appears.
        span: Span,
        /// Why it could not be resolved now.
        defers_on: Vec<DeferReason>,
    },
    /// `strategy.matrix: ${{ ... }}` evaluated to a value that isn't a
    /// documented matrix shape (an object of arrays, an object with
    /// `include`/`exclude`, or a bare array of standalone legs).
    #[error("{span}: strategy.matrix expression must evaluate to an object or array")]
    ExpressionNotMatrixShaped {
        /// The expression's span.
        span: Span,
    },
    /// `strategy.matrix: ${{ ... }}` depends on runtime-only data (e.g.
    /// `needs.<id>.outputs.<name>`) — dynamic matrices sourced from a
    /// previous job's output are a real, common GHA pattern, but Phase 1
    /// has no execution phase to produce that data, so this is out of v0
    /// scope rather than a guessed value.
    #[error(
        "{span}: strategy.matrix depends on runtime data not available at plan time in v0 (needs {defers_on:?})"
    )]
    DynamicMatrixNotSupported {
        /// The expression's span.
        span: Span,
        /// Why it could not be resolved now.
        defers_on: Vec<DeferReason>,
    },
    /// An embedded expression failed to parse or evaluate.
    #[error("{span}: {source}")]
    PartialEval {
        /// Where the offending value appears.
        span: Span,
        /// The underlying failure.
        #[source]
        source: PartialEvalError,
    },
}

/// Resolves `strategy:` (both the matrix and `fail-fast`/`max-parallel`)
/// into a [`StrategyPlan`], plus any lints raised along the way (dead
/// `exclude` entries).
pub(crate) fn plan_strategy(
    strategy: Option<&Strategy>,
    ctx: &FoldCtx<'_>,
    cap: usize,
) -> Result<(StrategyPlan, Vec<Lint>), MatrixError> {
    let Some(strategy) = strategy else {
        return Ok((
            StrategyPlan {
                is_matrix: false,
                fail_fast: true,
                max_parallel: None,
                legs: Vec::new(),
            },
            Vec::new(),
        ));
    };

    let fail_fast = fold_bool_default(strategy.fail_fast.as_ref(), ctx, true)?;
    let max_parallel = fold_max_parallel(strategy.max_parallel.as_ref(), ctx)?;

    let (is_matrix, legs, lints) = match &strategy.matrix {
        None => (false, Vec::new(), Vec::new()),
        Some(source) => {
            let (legs, lints) =
                expand::expand_matrix_source(&source.value, &source.span, ctx, cap)?;
            (true, legs, lints)
        }
    };

    Ok((
        StrategyPlan {
            is_matrix,
            fail_fast,
            max_parallel,
            legs,
        },
        lints,
    ))
}

fn fold_bool_default(
    v: Option<&greenlit_workflow::Spanned<ScalarOrExpr>>,
    ctx: &FoldCtx<'_>,
    default: bool,
) -> Result<bool, MatrixError> {
    let Some(v) = v else { return Ok(default) };
    let folded = fold_scalar_or_expr(&v.value, ctx).map_err(|source| MatrixError::PartialEval {
        span: v.span.clone(),
        source,
    })?;
    match folded {
        Folded::Value(value) => Ok(greenlit_expr::value::is_truthy(&value)),
        Folded::Residual { defers_on, .. } => Err(MatrixError::ValueNotStatic {
            span: v.span.clone(),
            defers_on: defers_on.into_iter().collect(),
        }),
    }
}

fn fold_max_parallel(
    v: Option<&greenlit_workflow::Spanned<ScalarOrExpr>>,
    ctx: &FoldCtx<'_>,
) -> Result<Option<NonZeroU32>, MatrixError> {
    let Some(v) = v else { return Ok(None) };
    let folded = fold_scalar_or_expr(&v.value, ctx).map_err(|source| MatrixError::PartialEval {
        span: v.span.clone(),
        source,
    })?;
    match folded {
        Folded::Value(value) => {
            let n = greenlit_expr::value::to_number(&value);
            Ok(NonZeroU32::new(n.max(0.0) as u32))
        }
        Folded::Residual { defers_on, .. } => Err(MatrixError::ValueNotStatic {
            span: v.span.clone(),
            defers_on: defers_on.into_iter().collect(),
        }),
    }
}

/// A matrix axis/`include`/`exclude` entry, already folded to
/// [`MatrixValue`] — the shape [`expand`]'s algorithm operates on,
/// independent of whether it came from an inline `strategy.matrix:`
/// mapping or a `${{ fromJSON(...) }}` expression's resulting object/array.
pub(crate) type ResolvedEntry = Vec<(String, MatrixValue)>;
