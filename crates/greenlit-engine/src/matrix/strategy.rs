//! Plan-time strategy state, including matrices that depend on completed
//! prerequisite outputs and therefore cannot expand during initial planning.

use std::num::NonZeroU32;

use greenlit_expr::Value;
use greenlit_expr::value::to_number;
use greenlit_workflow::model::job::{MatrixSource, Strategy};
use greenlit_workflow::model::value::ScalarOrExpr;
use greenlit_workflow::{Span, Spanned};
use serde::Serialize;

use crate::condition::DeferredExpr;
use crate::lints::Lint;
use crate::partial_eval::{FoldCtx, Folded, fold_scalar_or_expr, pretty_print};
use crate::planned::{Evaluation, Planned, scalar_source};

use super::deferred::{DeferredMatrixExpression, collect_deferred_matrix_expressions};
use super::{MatrixError, MatrixLeg, expand, value_kind_name};

/// A strategy control that is either known now or must be evaluated after
/// the job's direct dependencies finish.
#[derive(Debug, Clone, PartialEq)]
pub enum StrategyControl<T> {
    /// The declared value was fully resolved at plan time.
    Static(T),
    /// Runtime dependency data is still required. The authored value and
    /// residual expression are retained without inventing a fallback.
    Deferred(Planned<T>),
}

impl<T> StrategyControl<T> {
    /// Returns the static value, or `None` when it remains deferred.
    #[must_use]
    pub fn as_static(&self) -> Option<&T> {
        match self {
            Self::Static(value) => Some(value),
            Self::Deferred(_) => None,
        }
    }

    /// Returns whether runtime dependency data is still required.
    #[must_use]
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred(_))
    }
}

impl<T: Serialize> Serialize for StrategyControl<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Static(value) => value.serialize(serializer),
            Self::Deferred(value) => value.serialize(serializer),
        }
    }
}

/// Expansion state for a declared `strategy.matrix`.
///
/// A static matrix with no legs is intentionally different from a deferred
/// matrix. The former creates no job instances; the latter must be expanded
/// after its prerequisite outputs exist. GitHub documents output-produced
/// matrices as a supported workflow pattern.
/// <https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/run-job-variations#using-an-output-to-define-two-matrices>
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "evaluation", rename_all = "kebab-case")]
pub enum MatrixPlan {
    /// Every matrix value was known and the concrete legs were expanded.
    Static {
        /// Location of the authored `strategy.matrix` value.
        #[serde(serialize_with = "crate::json_shape::serialize_span")]
        span: Span,
        /// Concrete combinations in GitHub creation order.
        legs: Vec<MatrixLeg>,
    },
    /// One or more expressions require direct-dependency output data.
    Deferred {
        /// Location of the authored `strategy.matrix` value.
        #[serde(serialize_with = "crate::json_shape::serialize_span")]
        span: Span,
        /// Every runtime-dependent expression found in declaration order.
        expressions: Vec<DeferredMatrixExpression>,
        /// Parsed source retained as the Phase 2 materialization blueprint.
        /// It is an in-memory engine detail, not part of stable plan JSON.
        #[serde(skip)]
        source: MatrixSource,
    },
}

impl MatrixPlan {
    /// Concrete legs, or an empty slice while expansion is deferred.
    #[must_use]
    pub fn legs(&self) -> &[MatrixLeg] {
        match self {
            Self::Static { legs, .. } => legs,
            Self::Deferred { .. } => &[],
        }
    }

    /// Returns whether expansion awaits runtime dependency data.
    #[must_use]
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred { .. })
    }

    /// Deferred expressions in declaration order, or an empty slice for a
    /// matrix that was expanded statically.
    #[must_use]
    pub fn deferred_expressions(&self) -> &[DeferredMatrixExpression] {
        match self {
            Self::Static { .. } => &[],
            Self::Deferred { expressions, .. } => expressions,
        }
    }
}

/// `strategy:`, resolved as far as initial plan-time contexts permit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StrategyPlan {
    /// Default `true`. Runtime cancellation semantics belong to execution;
    /// this plan retains either the value or its dependency residual.
    pub fail_fast: StrategyControl<bool>,
    /// `None` means GitHub's default of the matrix job total. An authored
    /// value can itself remain deferred.
    pub max_parallel: Option<StrategyControl<NonZeroU32>>,
    /// `None` when no matrix was declared; otherwise explicit static or
    /// deferred expansion state.
    pub matrix: Option<MatrixPlan>,
}

impl StrategyPlan {
    /// Whether the job declared a matrix, including an empty or deferred one.
    #[must_use]
    pub fn is_matrix(&self) -> bool {
        self.matrix.is_some()
    }

    /// Whether a declared matrix still needs runtime materialization.
    #[must_use]
    pub fn is_matrix_deferred(&self) -> bool {
        self.matrix.as_ref().is_some_and(MatrixPlan::is_deferred)
    }

    /// Concrete legs for a static matrix, otherwise an empty slice.
    #[must_use]
    pub fn legs(&self) -> &[MatrixLeg] {
        self.matrix.as_ref().map_or(&[], MatrixPlan::legs)
    }

    /// Whether either strategy control still requires runtime data.
    #[must_use]
    pub fn has_deferred_controls(&self) -> bool {
        self.fail_fast.is_deferred()
            || self
                .max_parallel
                .as_ref()
                .is_some_and(StrategyControl::is_deferred)
    }
}

/// Materialize a matrix after all of its runtime dependencies are available.
///
/// Static plans are returned unchanged. Deferred plans re-evaluate their
/// retained authored source against the supplied runtime context and run the
/// same include/exclude/cartesian algorithm used during initial planning.
pub fn materialize_matrix(
    strategy: &StrategyPlan,
    ctx: &greenlit_expr::Context,
    cap: usize,
) -> Result<Vec<super::MatrixLeg>, MatrixError> {
    match &strategy.matrix {
        None => Ok(Vec::new()),
        Some(MatrixPlan::Static { legs, .. }) => Ok(legs.clone()),
        Some(MatrixPlan::Deferred { span, source, .. }) => {
            super::expand::expand_matrix_source_runtime(source, span, ctx, cap)
                .map(|(legs, _)| legs)
        }
    }
}

/// Resolve runtime-dependent matrix scheduling controls.
pub fn materialize_controls(
    strategy: &StrategyPlan,
    ctx: &greenlit_expr::Context,
) -> Result<(bool, Option<NonZeroU32>), MatrixError> {
    let fail_fast = match &strategy.fail_fast {
        StrategyControl::Static(value) => *value,
        StrategyControl::Deferred(planned) => {
            let crate::planned::Evaluation::Deferred(deferred) = &planned.evaluation else {
                return Ok((true, None));
            };
            match greenlit_expr::evaluate(&deferred.residual, ctx).map_err(|source| {
                MatrixError::PartialEval {
                    span: planned.span.clone(),
                    source: source.into(),
                }
            })? {
                Value::Bool(value) => value,
                actual => {
                    return Err(MatrixError::InvalidFailFastType {
                        actual: super::value_kind_name(&actual),
                        span: planned.span.clone(),
                    });
                }
            }
        }
    };
    let max_parallel = match &strategy.max_parallel {
        None => None,
        Some(StrategyControl::Static(value)) => Some(*value),
        Some(StrategyControl::Deferred(planned)) => {
            let crate::planned::Evaluation::Deferred(deferred) = &planned.evaluation else {
                return Ok((fail_fast, None));
            };
            let value = greenlit_expr::evaluate(&deferred.residual, ctx).map_err(|source| {
                MatrixError::PartialEval {
                    span: planned.span.clone(),
                    source: source.into(),
                }
            })?;
            if !matches!(value, Value::Number(_)) {
                return Err(MatrixError::InvalidMaxParallelType {
                    actual: super::value_kind_name(&value),
                    span: planned.span.clone(),
                });
            }
            let number = to_number(&value);
            if !number.is_finite()
                || number.fract() != 0.0
                || number < 1.0
                || number > f64::from(u32::MAX)
            {
                return Err(MatrixError::InvalidMaxParallelValue {
                    value: number,
                    span: planned.span.clone(),
                });
            }
            Some(NonZeroU32::new(number as u32).ok_or_else(|| {
                MatrixError::InvalidMaxParallelValue {
                    value: number,
                    span: planned.span.clone(),
                }
            })?)
        }
    };
    Ok((fail_fast, max_parallel))
}

/// Resolves `strategy:` and preserves runtime-dependent matrix/control
/// expressions for later materialization.
pub(crate) fn plan_strategy(
    strategy: Option<&Strategy>,
    ctx: &FoldCtx<'_>,
    cap: usize,
) -> Result<(StrategyPlan, Vec<Lint>), MatrixError> {
    let Some(strategy) = strategy else {
        return Ok((
            StrategyPlan {
                fail_fast: StrategyControl::Static(true),
                max_parallel: None,
                matrix: None,
            },
            Vec::new(),
        ));
    };

    let fail_fast = fold_bool_default(strategy.fail_fast.as_ref(), ctx, true)?;
    let max_parallel = fold_max_parallel(strategy.max_parallel.as_ref(), ctx)?;

    let (matrix, lints) = match &strategy.matrix {
        None => (None, Vec::new()),
        Some(source) => {
            let expressions = collect_deferred_matrix_expressions(&source.value, ctx)?;
            if expressions.is_empty() {
                let (legs, lints) =
                    expand::expand_matrix_source(&source.value, &source.span, ctx, cap)?;
                (
                    Some(MatrixPlan::Static {
                        span: source.span.clone(),
                        legs,
                    }),
                    lints,
                )
            } else {
                expand::validate_static_fragments(&source.value, &source.span, ctx, cap)?;
                (
                    Some(MatrixPlan::Deferred {
                        span: source.span.clone(),
                        expressions,
                        source: source.value.clone(),
                    }),
                    Vec::new(),
                )
            }
        }
    };

    Ok((
        StrategyPlan {
            fail_fast,
            max_parallel,
            matrix,
        },
        lints,
    ))
}

fn fold_bool_default(
    value: Option<&Spanned<ScalarOrExpr>>,
    ctx: &FoldCtx<'_>,
    default: bool,
) -> Result<StrategyControl<bool>, MatrixError> {
    let Some(value) = value else {
        return Ok(StrategyControl::Static(default));
    };
    let folded =
        fold_scalar_or_expr(&value.value, ctx).map_err(|source| MatrixError::PartialEval {
            span: value.span.clone(),
            source,
        })?;
    match folded {
        Folded::Value(Value::Bool(value)) => Ok(StrategyControl::Static(value)),
        Folded::Value(value_kind) => Err(MatrixError::InvalidFailFastType {
            actual: value_kind_name(&value_kind),
            span: value.span.clone(),
        }),
        Folded::Residual { expr, defers_on } => Ok(StrategyControl::Deferred(Planned {
            span: value.span.clone(),
            source: scalar_source(&value.value),
            evaluation: Evaluation::Deferred(DeferredExpr {
                residual_text: pretty_print(&expr),
                residual: expr,
                defers_on: defers_on.into_iter().collect(),
            }),
        })),
    }
}

fn fold_max_parallel(
    value: Option<&Spanned<ScalarOrExpr>>,
    ctx: &FoldCtx<'_>,
) -> Result<Option<StrategyControl<NonZeroU32>>, MatrixError> {
    let Some(value) = value else { return Ok(None) };
    let folded =
        fold_scalar_or_expr(&value.value, ctx).map_err(|source| MatrixError::PartialEval {
            span: value.span.clone(),
            source,
        })?;
    match folded {
        Folded::Value(Value::Number(number))
            if number.is_finite()
                && number >= 1.0
                && number <= f64::from(u32::MAX)
                && number.fract() == 0.0 =>
        {
            let Some(number) = NonZeroU32::new(number as u32) else {
                return Err(MatrixError::InvalidMaxParallelValue {
                    value: number,
                    span: value.span.clone(),
                });
            };
            Ok(Some(StrategyControl::Static(number)))
        }
        Folded::Value(Value::Number(number)) => Err(MatrixError::InvalidMaxParallelValue {
            value: number,
            span: value.span.clone(),
        }),
        Folded::Value(value_kind) => Err(MatrixError::InvalidMaxParallelType {
            actual: value_kind_name(&value_kind),
            span: value.span.clone(),
        }),
        Folded::Residual { expr, defers_on } => Ok(Some(StrategyControl::Deferred(Planned {
            span: value.span.clone(),
            source: scalar_source(&value.value),
            evaluation: Evaluation::Deferred(DeferredExpr {
                residual_text: pretty_print(&expr),
                residual: expr,
                defers_on: defers_on.into_iter().collect(),
            }),
        }))),
    }
}
