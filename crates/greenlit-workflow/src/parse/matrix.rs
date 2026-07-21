//! `strategy:` and `strategy.matrix:`.

use crate::error::ParseError;
use crate::expression::single_expression_body;
use crate::model::job::{Matrix, MatrixEntry, MatrixSource, Strategy};
use crate::model::value::ScalarOrExpr;
use crate::parse::util::{
    as_mapping, as_sequence, find, key_text, reject_unknown_keys, scalar_or_expr,
    spanned_scalar_or_expr, to_yaml_value,
};
use crate::span::Spanned;
use crate::yaml::raw::RawNode;

const STRATEGY_KEYS: &[&str] = &["matrix", "fail-fast", "max-parallel"];

pub(crate) fn parse_strategy(node: &Spanned<RawNode>) -> Result<Strategy, ParseError> {
    let entries = as_mapping(node, "strategy")?;
    reject_unknown_keys(entries, STRATEGY_KEYS, "strategy")?;
    let matrix = find(entries, "matrix")
        .map(parse_matrix_source)
        .transpose()?;
    let fail_fast = find(entries, "fail-fast")
        .map(|v| spanned_scalar_or_expr(v, "strategy.fail-fast"))
        .transpose()?;
    let max_parallel = find(entries, "max-parallel")
        .map(|v| spanned_scalar_or_expr(v, "strategy.max-parallel"))
        .transpose()?;
    Ok(Strategy {
        matrix,
        fail_fast,
        max_parallel,
    })
}

fn parse_matrix_source(node: &Spanned<RawNode>) -> Result<Spanned<MatrixSource>, ParseError> {
    match &node.value {
        RawNode::Mapping(_) => {
            let matrix = parse_matrix(node)?;
            Ok(Spanned::new(
                MatrixSource::Inline(matrix),
                node.span.clone(),
            ))
        }
        RawNode::Scalar(_) => match scalar_or_expr(node, "strategy.matrix")? {
            ScalarOrExpr::Expression(text) => {
                let expr = Spanned::new(text, node.span.clone());
                Ok(Spanned::new(
                    MatrixSource::Expression(expr),
                    node.span.clone(),
                ))
            }
            ScalarOrExpr::Literal(_) => Err(ParseError::Schema {
                span: node.span.clone(),
                message: "strategy.matrix must be a mapping or a '${{ }}' expression".to_owned(),
            }),
        },
        RawNode::Sequence(_) => Err(ParseError::Schema {
            span: node.span.clone(),
            message: "strategy.matrix must be a mapping or a '${{ }}' expression".to_owned(),
        }),
    }
}

fn parse_matrix(node: &Spanned<RawNode>) -> Result<Matrix, ParseError> {
    let entries = as_mapping(node, "strategy.matrix")?;
    let mut axes = Vec::new();
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for (k, v) in entries {
        let name = key_text(k)?;
        match name {
            "include" => include = parse_matrix_entries(v, "strategy.matrix.include")?,
            "exclude" => exclude = parse_matrix_entries(v, "strategy.matrix.exclude")?,
            _ => {
                // GitHub documents context-valued axis arrays directly,
                // including `version: ${{ github.event.client_payload.versions }}`:
                // https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/run-job-variations#using-contexts-to-create-matrices
                // Keep that single expression as the axis source so the
                // engine can expand its eventual array value. Literal axes
                // still have to be YAML sequences.
                let values = match &v.value {
                    RawNode::Sequence(items) => items
                        .iter()
                        .map(|item| Spanned::new(to_yaml_value(item), item.span.clone()))
                        .collect(),
                    RawNode::Scalar(_) => match scalar_or_expr(v, "strategy.matrix.<axis>")? {
                        ScalarOrExpr::Expression(ref text)
                            if single_expression_body(text).is_some() =>
                        {
                            vec![Spanned::new(to_yaml_value(v), v.span.clone())]
                        }
                        ScalarOrExpr::Expression(_) | ScalarOrExpr::Literal(_) => {
                            return Err(ParseError::Schema {
                                span: v.span.clone(),
                                message: "strategy.matrix.<axis> must be a sequence or a single '${{ }}' expression"
                                    .to_owned(),
                            });
                        }
                    },
                    RawNode::Mapping(_) => {
                        return Err(ParseError::Schema {
                            span: v.span.clone(),
                            message: "strategy.matrix.<axis> must be a sequence or a single '${{ }}' expression"
                                .to_owned(),
                        });
                    }
                };
                axes.push((Spanned::new(name.to_owned(), k.span.clone()), values));
            }
        }
    }
    Ok(Matrix {
        axes,
        include,
        exclude,
    })
}

fn parse_matrix_entries(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<Vec<Spanned<MatrixEntry>>, ParseError> {
    let items = as_sequence(node, context)?;
    items
        .iter()
        .map(|item| {
            let entries = as_mapping(item, context)?;
            let mut pairs = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                pairs.push((
                    Spanned::new(key_text(k)?.to_owned(), k.span.clone()),
                    Spanned::new(to_yaml_value(v), v.span.clone()),
                ));
            }
            Ok(Spanned::new(pairs, item.span.clone()))
        })
        .collect()
}
