//! Parses `inputs:` and `outputs:`.

use indexmap::IndexMap;

use crate::manifest::yaml::RawNode;
use crate::manifest::{ActionInput, ActionOutput, ManifestError};
use greenlit_workflow::Spanned;

use super::util::{as_mapping, find, optional_string, reject_unknown_keys, scalar_bool};

const INPUT_KEYS: &[&str] = &["description", "required", "default", "deprecationMessage"];
const OUTPUT_KEYS: &[&str] = &["description", "value"];

pub(super) fn parse_inputs(
    node: &Spanned<RawNode>,
) -> Result<IndexMap<String, ActionInput>, ManifestError> {
    let entries = as_mapping(node, "inputs")?;
    let mut inputs = IndexMap::with_capacity(entries.len());
    for (key, value) in entries {
        let Some(id) = key.value.as_key_text() else {
            return Err(ManifestError::NonScalarKey {
                span: key.span.clone(),
            });
        };
        let context = format!("inputs.{id}");
        let field_entries = as_mapping(value, &context)?;
        reject_unknown_keys(field_entries, INPUT_KEYS, &context)?;

        let required = match find(field_entries, "required") {
            Some(node) => scalar_bool(node, &format!("{context}.required"))?,
            None => false,
        };

        inputs.insert(
            id.to_owned(),
            ActionInput {
                description: optional_string(field_entries, "description", &context)?,
                required,
                default: optional_string(field_entries, "default", &context)?,
                deprecation_message: optional_string(
                    field_entries,
                    "deprecationMessage",
                    &context,
                )?,
            },
        );
    }
    Ok(inputs)
}

pub(super) fn parse_outputs(
    node: &Spanned<RawNode>,
) -> Result<IndexMap<String, ActionOutput>, ManifestError> {
    let entries = as_mapping(node, "outputs")?;
    let mut outputs = IndexMap::with_capacity(entries.len());
    for (key, value) in entries {
        let Some(id) = key.value.as_key_text() else {
            return Err(ManifestError::NonScalarKey {
                span: key.span.clone(),
            });
        };
        let context = format!("outputs.{id}");
        let field_entries = as_mapping(value, &context)?;
        reject_unknown_keys(field_entries, OUTPUT_KEYS, &context)?;
        outputs.insert(
            id.to_owned(),
            ActionOutput {
                description: optional_string(field_entries, "description", &context)?,
                value: optional_string(field_entries, "value", &context)?,
            },
        );
    }
    Ok(outputs)
}
