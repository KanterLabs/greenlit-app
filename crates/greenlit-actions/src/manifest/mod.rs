//! `action.yml`/`action.yaml` parsing.
//!
//! `PHASE-3-actions.md`: "Parse `action.yml`/`action.yaml`: `name`,
//! `description`, `inputs` (default/required/deprecationMessage), `outputs`
//! (incl. composite `value`), `runs` — `using: node20|node24|composite|docker`,
//! `main`, `pre`, `post`, `pre-if`, `post-if`, composite `steps`, docker
//! `image`/`args`/`entrypoint`/`env`."
//!
//! This module produces a *structural* model only — every field GitHub
//! documents, typed, with `${{ }}` expression text preserved verbatim
//! wherever GitHub allows an expression (composite steps' `if`/`run`/`env`/
//! `with`, `runs.pre-if`/`post-if`). Evaluating those expressions and
//! actually executing composite/JS/Docker actions is explicitly the next
//! Phase 3 task group's job ("Action execution"), not this one
//! ("Action resolution") — see this crate's top-level docs.
//!
//! # Deliberate scope decisions
//!
//! - GitHub's own metadata-syntax documentation marks `name`/`description`
//!   (top level) and `description` (per input) as "required" — but that
//!   requirement is enforced for GitHub Marketplace *publishing*, not for
//!   the runner *executing* an action, and real-world internal/composite
//!   actions in the wild routinely omit them and still run. This crate
//!   therefore only hard-requires `runs:` (execution genuinely cannot
//!   proceed without it) and treats every other documented "required"
//!   field as optional. Flagged per `AGENTS.md`: "Every conflict found gets
//!   flagged in the phase summary."
//! - `outputs.<id>.value` (composite-only) is modeled as optional at the
//!   type level rather than cross-validated against `runs.using ==
//!   composite`; a composite action whose declared output has no `value`
//!   fails at evaluation time (the execution task group's concern), which
//!   is exactly how GitHub itself would behave.

mod parse;
mod yaml;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use greenlit_workflow::Span;
use indexmap::IndexMap;

pub use parse::parse_manifest;

/// A file this crate will never accept: bounds a hostile/corrupt
/// `action.yml` before it is even handed to the YAML parser. Real manifests
/// are metadata files a few kilobytes long at most.
pub const MAX_MANIFEST_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// A fully parsed `action.yml`/`action.yaml`.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionManifest {
    /// The action's display name, if declared.
    pub name: Option<String>,
    /// The action's description, if declared.
    pub description: Option<String>,
    /// Declared inputs, in authored order, keyed by input id.
    pub inputs: IndexMap<String, ActionInput>,
    /// Declared outputs, in authored order, keyed by output id.
    pub outputs: IndexMap<String, ActionOutput>,
    /// How the action runs.
    pub runs: Runs,
}

/// One `inputs.<id>` entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActionInput {
    /// Human-readable description, if declared.
    pub description: Option<String>,
    /// Whether the input is required. GitHub defaults this to `false` when
    /// omitted.
    pub required: bool,
    /// The default value substituted when the workflow does not supply
    /// this input, if declared.
    pub default: Option<String>,
    /// A deprecation warning to surface when this input is used, if
    /// declared.
    pub deprecation_message: Option<String>,
}

/// One `outputs.<id>` entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActionOutput {
    /// Human-readable description, if declared.
    pub description: Option<String>,
    /// Composite-actions-only: the (possibly `${{ }}`-templated, preserved
    /// verbatim) text producing this output's value.
    pub value: Option<String>,
}

/// The `runs:` block — how GitHub (and Greenlit) executes this action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runs {
    /// `using: node20` / `using: node24`.
    Node(NodeRuns),
    /// `using: composite`.
    Composite {
        /// The composite action's own steps, in authored order.
        steps: Vec<CompositeStep>,
    },
    /// `using: docker`.
    Docker(DockerRuns),
}

/// A JavaScript action's `runs:` fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRuns {
    /// Which pinned Node runtime this action targets.
    pub using: NodeVersion,
    /// Entry point script path, relative to the action's root.
    pub main: String,
    /// Setup script path, if declared.
    pub pre: Option<String>,
    /// Condition (raw, possibly a `${{ }}` expression) gating whether `pre`
    /// runs, if declared.
    pub pre_if: Option<String>,
    /// Cleanup script path, if declared.
    pub post: Option<String>,
    /// Condition (raw, possibly a `${{ }}` expression) gating whether
    /// `post` runs, if declared.
    pub post_if: Option<String>,
}

/// The two Node runtimes `PHASE-3-actions.md` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeVersion {
    /// `using: node20`.
    Node20,
    /// `using: node24`.
    Node24,
}

/// A Docker action's `runs:` fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerRuns {
    /// The image reference: a `Dockerfile`-relative path to build, or a
    /// `docker://image:tag` reference to pull — which of the two is this
    /// crate's caller's concern (see the type's use sites), not something
    /// this crate disambiguates itself.
    pub image: String,
    /// Arguments passed to the container's entrypoint, in order (each
    /// possibly a `${{ }}` expression, preserved verbatim).
    pub args: Vec<String>,
    /// Overrides the image's entrypoint, if declared.
    pub entrypoint: Option<String>,
    /// Environment variables set on the container, in authored order
    /// (values preserved verbatim).
    pub env: IndexMap<String, String>,
}

/// One step of a composite action's `runs.steps`.
///
/// Exactly one of [`CompositeStep::run`] or [`CompositeStep::uses`] is
/// ever populated, mirroring GitHub's own step schema (a step either runs a
/// shell command or invokes another action, never both, never neither) —
/// enforced by [`parse_manifest`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompositeStep {
    /// This step's id, if declared (referenced by later steps' `${{
    /// steps.<id>.outputs.* }}`).
    pub id: Option<String>,
    /// Display name, if declared.
    pub name: Option<String>,
    /// Raw `if:` condition text, if declared.
    pub if_condition: Option<String>,
    /// A shell command to run (mutually exclusive with `uses`).
    pub run: Option<String>,
    /// Required alongside `run`; which shell interprets it.
    pub shell: Option<String>,
    /// Working directory for `run`, if declared.
    pub working_directory: Option<String>,
    /// Step-level environment variables, in authored order (values
    /// preserved verbatim).
    pub env: IndexMap<String, String>,
    /// A nested action reference to invoke (mutually exclusive with `run`).
    pub uses: Option<String>,
    /// Inputs passed to a nested `uses` action, in authored order (values
    /// preserved verbatim).
    pub with: IndexMap<String, String>,
    /// Raw `continue-on-error` text (a literal `true`/`false` or a `${{ }}`
    /// expression), if declared.
    pub continue_on_error: Option<String>,
}

/// Tries `action.yml` then `action.yaml` inside `action_dir` and parses
/// whichever is present.
///
/// # Errors
/// Returns [`ManifestError::NotFound`] if neither file exists, or any other
/// [`ManifestError`] from reading/parsing whichever one does.
pub fn load_manifest(action_dir: &Path) -> Result<ActionManifest, ManifestError> {
    for filename in ["action.yml", "action.yaml"] {
        let candidate = action_dir.join(filename);
        match std::fs::read_to_string(&candidate) {
            Ok(source) => {
                let label: Arc<str> = Arc::from(candidate.display().to_string());
                return parse_manifest(label, &source);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ManifestError::Io {
                    path: candidate,
                    message: error.to_string(),
                });
            }
        }
    }
    Err(ManifestError::NotFound {
        dir: action_dir.to_path_buf(),
    })
}

/// A failure parsing an `action.yml`/`action.yaml`.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Neither `action.yml` nor `action.yaml` exists in the action's
    /// directory.
    #[error("no action.yml or action.yaml found in {}", dir.display())]
    NotFound {
        /// The directory that was checked.
        dir: PathBuf,
    },
    /// Reading the manifest file failed for a reason other than "it does
    /// not exist" (already handled by trying the other filename).
    #[error("could not read {}: {message}", path.display())]
    Io {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error's message.
        message: String,
    },
    /// The manifest source exceeds [`MAX_MANIFEST_SOURCE_BYTES`].
    #[error("{path} exceeds Greenlit's {MAX_MANIFEST_SOURCE_BYTES}-byte manifest size limit")]
    SourceLimit {
        /// The manifest file.
        path: Arc<str>,
    },
    /// Malformed YAML syntax.
    #[error("{path}:{line}:{column}: {message}")]
    Yaml {
        /// The manifest file.
        path: Arc<str>,
        /// 1-based line.
        line: u32,
        /// 1-based column.
        column: u32,
        /// The parser's diagnostic.
        message: String,
    },
    /// The file contained more or less than exactly one YAML document.
    #[error("{path} contains {count} YAML documents; expected exactly one")]
    MultipleDocuments {
        /// The manifest file.
        path: Arc<str>,
        /// How many documents were found.
        count: usize,
    },
    /// The file contained no YAML document at all.
    #[error("{path} is empty")]
    EmptyDocument {
        /// The manifest file.
        path: Arc<str>,
    },
    /// A mapping repeated the same key twice (case-insensitively), matching
    /// the runner's `TemplateReader` duplicate-key rejection.
    #[error("{span}: duplicate key '{key}' (first used at {first_span})")]
    DuplicateKey {
        /// The duplicate's location.
        span: Span,
        /// The repeated key text.
        key: String,
        /// Where the key was first used.
        first_span: Span,
    },
    /// A mapping key was not a scalar.
    #[error("{span}: mapping keys must be scalars")]
    NonScalarKey {
        /// The offending key's location.
        span: Span,
    },
    /// A YAML anchor/alias was used; this crate's manifest parser does not
    /// support them (see module docs).
    #[error("{span}: YAML anchors/aliases are not supported in action.yml")]
    AnchorsNotSupported {
        /// The offending node's location.
        span: Span,
    },
    /// An explicit YAML tag (`!!str`, a custom tag, …) was used; this
    /// crate's manifest parser does not support them (see module docs).
    #[error("{span}: explicit YAML tags are not supported in action.yml")]
    ExplicitTagsNotSupported {
        /// The offending node's location.
        span: Span,
    },
    /// The document exceeded a bounded YAML structural limit (depth, node
    /// count, or result size).
    #[error("{span}: {message}")]
    YamlLimit {
        /// The location the limit was hit at.
        span: Span,
        /// The limit that was exceeded.
        message: String,
    },
    /// A node's shape did not match what was expected there (e.g. a
    /// mapping expected where a scalar was written).
    #[error("{span}: {message}")]
    Schema {
        /// The offending node's location.
        span: Span,
        /// What was expected.
        message: String,
    },
    /// A mapping contained a key this crate's manifest schema does not
    /// recognize.
    #[error("{span}: unknown key '{key}' in {context}")]
    UnknownKey {
        /// The key's location.
        span: Span,
        /// The unrecognized key text.
        key: String,
        /// Which construct was being parsed.
        context: String,
    },
    /// A required key was absent.
    #[error("{span}: {context} is missing required key '{key}'")]
    MissingKey {
        /// The enclosing mapping's location.
        span: Span,
        /// The missing key's name.
        key: &'static str,
        /// Which construct was being parsed.
        context: String,
    },
    /// `runs.using` named something other than `node20`, `node24`,
    /// `composite`, or `docker`.
    #[error(
        "{span}: unsupported 'using: {value}' (Greenlit supports node20, node24, composite, docker)"
    )]
    UnsupportedUsing {
        /// The offending value's location.
        span: Span,
        /// The unrecognized value.
        value: String,
    },
    /// A composite step declared neither `run` nor `uses`.
    #[error("{span}: composite step must have either 'run' or 'uses'")]
    CompositeStepMissingRunOrUses {
        /// The step's location.
        span: Span,
    },
    /// A composite step declared both `run` and `uses`.
    #[error("{span}: composite step must not have both 'run' and 'uses'")]
    CompositeStepHasBothRunAndUses {
        /// The step's location.
        span: Span,
    },
    /// A composite step declared `run` without the `shell` GitHub requires
    /// alongside it.
    #[error("{span}: composite step with 'run' is missing required key 'shell'")]
    CompositeStepRunMissingShell {
        /// The step's location.
        span: Span,
    },
}
