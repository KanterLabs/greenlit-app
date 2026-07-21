//! `container:` and `services.<id>:` — identical shape, shared parsing.

use crate::error::ParseError;
use crate::model::job::{ContainerSpec, Credentials};
use crate::parse::util::{
    as_mapping, find, reject_unknown_keys, require, scalar_or_expr_map, scalar_or_expr_seq,
    spanned_scalar_or_expr,
};
use crate::span::Spanned;
use crate::yaml::raw::RawNode;

const CONTAINER_KEYS: &[&str] = &["image", "credentials", "env", "ports", "volumes", "options"];
const CREDENTIALS_KEYS: &[&str] = &["username", "password"];

/// Parse a `container:`/`services.<id>:` value, which may be a bare image
/// string shorthand or a full mapping.
pub(crate) fn parse_container(
    node: &Spanned<RawNode>,
    context: &str,
) -> Result<ContainerSpec, ParseError> {
    match &node.value {
        RawNode::Scalar(_) => Ok(ContainerSpec {
            image: spanned_scalar_or_expr(node, context)?,
            credentials: None,
            env: Vec::new(),
            ports: Vec::new(),
            volumes: Vec::new(),
            options: None,
        }),
        RawNode::Mapping(_) => {
            let entries = as_mapping(node, context)?;
            reject_unknown_keys(entries, CONTAINER_KEYS, context)?;
            let image_node = require(entries, "image", &node.span, context)?;
            let image = spanned_scalar_or_expr(image_node, context)?;
            let credentials = match find(entries, "credentials") {
                Some(v) => Some(Spanned::new(parse_credentials(v, context)?, v.span.clone())),
                None => None,
            };
            let env = match find(entries, "env") {
                Some(v) => scalar_or_expr_map(v, context)?,
                None => Vec::new(),
            };
            let ports = match find(entries, "ports") {
                Some(v) => scalar_or_expr_seq(v, context)?,
                None => Vec::new(),
            };
            let volumes = match find(entries, "volumes") {
                Some(v) => scalar_or_expr_seq(v, context)?,
                None => Vec::new(),
            };
            let options = find(entries, "options")
                .map(|v| spanned_scalar_or_expr(v, context))
                .transpose()?;
            Ok(ContainerSpec {
                image,
                credentials,
                env,
                ports,
                volumes,
                options,
            })
        }
        RawNode::Sequence(_) => Err(ParseError::Schema {
            span: node.span.clone(),
            message: format!("{context} must be a string or a mapping"),
        }),
    }
}

fn parse_credentials(node: &Spanned<RawNode>, context: &str) -> Result<Credentials, ParseError> {
    let entries = as_mapping(node, context)?;
    reject_unknown_keys(entries, CREDENTIALS_KEYS, context)?;
    let username = find(entries, "username")
        .map(|v| spanned_scalar_or_expr(v, context))
        .transpose()?;
    let password = find(entries, "password")
        .map(|v| spanned_scalar_or_expr(v, context))
        .transpose()?;
    Ok(Credentials { username, password })
}
