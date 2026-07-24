//! Top-level `action.yml` dispatch: `name`/`description`/`inputs`/
//! `outputs`/`runs`, delegating the two structured blocks to
//! [`inputs`]/[`runs`].

mod inputs;
mod runs;
mod util;

use std::sync::Arc;

use indexmap::IndexMap;

use crate::manifest::yaml::parse_raw;
use crate::manifest::{ActionManifest, MAX_MANIFEST_SOURCE_BYTES, ManifestError};

use util::{as_mapping, optional_string, reject_unknown_keys};

/// The manifest's own top-level keys this crate models, plus two
/// marketplace-publishing-only keys (`author`, `branding`) recognized and
/// ignored rather than rejected — see this module's parent doc comment
/// ("Deliberate scope decisions") for why: real-world `action.yml` files
/// (this crate's whole reason for existing) use `branding:` near-
/// universally, unlike `greenlit-workflow`'s stricter "even real unmodeled
/// keys are an error" policy for *authored* workflow YAML.
const TOP_LEVEL_KEYS: &[&str] = &[
    "name",
    "description",
    "inputs",
    "outputs",
    "runs",
    "author",
    "branding",
];

/// Parses `source` (the contents of an `action.yml`/`action.yaml`; `path`
/// labels it in error messages) into a typed [`ActionManifest`].
///
/// # Errors
/// Returns [`ManifestError::SourceLimit`] if `source` exceeds
/// [`MAX_MANIFEST_SOURCE_BYTES`], or any other [`ManifestError`] raised
/// while parsing the YAML or validating its shape against the schema this
/// crate models.
pub fn parse_manifest(
    path: impl Into<Arc<str>>,
    source: &str,
) -> Result<ActionManifest, ManifestError> {
    let path = path.into();
    if source.len() > MAX_MANIFEST_SOURCE_BYTES {
        return Err(ManifestError::SourceLimit { path });
    }
    let root = parse_raw(path, source)?;
    let entries = as_mapping(&root, "action manifest")?;
    reject_unknown_keys(entries, TOP_LEVEL_KEYS, "action manifest")?;

    let runs_node = util::require(entries, "runs", &root.span, "action manifest")?;

    Ok(ActionManifest {
        name: optional_string(entries, "name", "action manifest")?,
        description: optional_string(entries, "description", "action manifest")?,
        inputs: match util::find(entries, "inputs") {
            Some(node) => inputs::parse_inputs(node)?,
            None => IndexMap::new(),
        },
        outputs: match util::find(entries, "outputs") {
            Some(node) => inputs::parse_outputs(node)?,
            None => IndexMap::new(),
        },
        runs: runs::parse_runs(runs_node)?,
    })
}
