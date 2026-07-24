//! Command-line argument definitions for `litci` (clap derive types).
//!
//! Kept separate from dispatch logic so the argument *shape* -- which flags
//! exist and how each one parses -- is reviewable on its own.
//! `PHASE-1-engine-core.md` greenlit-app section: "litci plan [--json] [-e
//! EVENT] [-W path] [--var KEY=VALUE]"; `litci stats` takes no flags.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// `litci` -- run your GitHub Actions workflows locally, fast, with results
/// you can trust (`greenlit-v0-spec.md`). `clean` remains unimplemented.
#[derive(Debug, Parser)]
#[command(name = "litci", version, about, long_about = None)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Print the fully resolved execution plan for a workflow -- no
    /// containers, no network.
    Plan(PlanArgs),
    /// Run a shell-only workflow end to end in isolated containers.
    Run(RunArgs),
    /// Detect, start, or install the container engine (three-state UX).
    Setup(SetupArgs),
    /// Authenticate for GitHub configuration-variable lookup and action
    /// resolution: GitHub App device flow by default, or `--pat`/`--gh`.
    Auth(AuthArgs),
    /// Show local invocation history and per-stage timing trends. Read-only:
    /// never appends a metrics record for its own invocation.
    Stats,
}

#[derive(Debug, clap::Args)]
pub(crate) struct AuthArgs {
    /// Paste a fine-grained personal access token instead of using the
    /// device flow.
    #[arg(long = "pat", conflicts_with = "gh")]
    pub(crate) pat: bool,

    /// Use `gh auth token` (the GitHub CLI's already-authenticated token)
    /// instead of the device flow. Prints a broad-scope warning: a
    /// `gh`-issued token typically carries more than read-only access.
    #[arg(long = "gh", conflicts_with = "pat")]
    pub(crate) gh: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct RunArgs {
    /// Which trigger event to simulate.
    #[arg(short = 'e', long = "event", value_enum, default_value = "push")]
    pub(crate) event: EventArg,

    /// Path to the workflow YAML file to run. When omitted, Greenlit looks for
    /// exactly one `*.yml`/`*.yaml` file under `.github/workflows/`.
    #[arg(short = 'W', long = "workflow")]
    pub(crate) workflow: Option<PathBuf>,

    /// Run only this job and its transitive `needs:` dependencies.
    #[arg(short = 'j', long = "job")]
    pub(crate) job: Option<String>,

    /// A local variable override, `KEY=VALUE`. Repeatable.
    #[arg(long = "var", value_name = "KEY=VALUE", value_parser = parse_var)]
    pub(crate) vars: Vec<(String, String)>,

    /// A `workflow_dispatch` input, `KEY=VALUE`. Repeatable.
    #[arg(long = "input", value_name = "KEY=VALUE", value_parser = parse_key_val)]
    pub(crate) inputs: Vec<(String, String)>,

    /// A secret value override, `KEY=VALUE`. Repeatable; the highest
    /// priority source in the `secrets.*` resolution chain (CLI override,
    /// then same-named process environment variable, then
    /// `.litci/secrets`, then an interactive prompt for anything still
    /// unresolved). Every resolved value is registered for log masking
    /// before any step runs.
    #[arg(short = 's', long = "secret", value_name = "KEY=VALUE", value_parser = parse_secret)]
    pub(crate) secrets: Vec<(String, String)>,

    /// Workspace isolation mechanism.
    #[arg(long = "isolation", value_enum, default_value = "auto")]
    pub(crate) isolation: IsolationArg,

    /// After the run, export each ran job's overlay changes, list them, and
    /// (after confirmation) apply them to the working tree. Requires overlay
    /// isolation — refused together with `--isolation copy-in`, and with
    /// `--no-input` (the confirmation cannot be skipped in v0). Implies
    /// `--isolation overlay`: the run fails if overlay cannot be mounted
    /// rather than falling back to copy-in and discarding the changes.
    #[arg(long = "write-back")]
    pub(crate) write_back: bool,

    /// Disable every interactive prompt. Conflicts with `--write-back`.
    #[arg(long = "no-input")]
    pub(crate) no_input: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct SetupArgs {
    /// Pre-confirm the single install/start prompt (non-interactive).
    #[arg(short = 'y', long = "yes")]
    pub(crate) yes: bool,
}

/// Which workspace isolation mechanism `litci run` requests of `greenlit-init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum IsolationArg {
    /// Try overlay, fall back to copy-in (the default).
    #[value(name = "auto")]
    Auto,
    /// Require overlay; fail if it cannot be mounted.
    #[value(name = "overlay")]
    Overlay,
    /// Always copy the checkout in.
    #[value(name = "copy-in")]
    CopyIn,
}

impl From<IsolationArg> for greenlit_runtime::IsolationStrategy {
    fn from(value: IsolationArg) -> Self {
        match value {
            IsolationArg::Auto => greenlit_runtime::IsolationStrategy::Auto,
            IsolationArg::Overlay => greenlit_runtime::IsolationStrategy::Overlay,
            IsolationArg::CopyIn => greenlit_runtime::IsolationStrategy::CopyIn,
        }
    }
}

#[derive(Debug, clap::Args)]
pub(crate) struct PlanArgs {
    /// Emit the stable JSON plan document to stdout instead of the
    /// human-readable tree. Diagnostics/timings still go to stderr.
    #[arg(long)]
    pub(crate) json: bool,

    /// Which trigger event to simulate.
    #[arg(short = 'e', long = "event", value_enum, default_value = "push")]
    pub(crate) event: EventArg,

    /// Path to the workflow YAML file to plan. When omitted, Greenlit looks
    /// for exactly one `*.yml`/`*.yaml` file under `.github/workflows/`.
    #[arg(short = 'W', long = "workflow")]
    pub(crate) workflow: Option<PathBuf>,

    /// A local variable override, `KEY=VALUE`. Repeatable; the highest
    /// priority source in the `vars.*` resolution chain (CLI override, then
    /// same-named process environment variable, then `.litci/vars`).
    #[arg(long = "var", value_name = "KEY=VALUE", value_parser = parse_var)]
    pub(crate) vars: Vec<(String, String)>,

    /// A `workflow_dispatch` input, `KEY=VALUE`. Repeatable; later entries
    /// for the same key win.
    #[arg(long = "input", value_name = "KEY=VALUE", value_parser = parse_key_val)]
    pub(crate) inputs: Vec<(String, String)>,
}

/// Which synthetic trigger event `-e`/`--event` selects -- mirrors
/// [`greenlit_engine::EventKind`] one-to-one. Kept as its own clap-derived
/// type (rather than deriving `ValueEnum` on the engine's own enum, which
/// would pull a `clap` dependency into `greenlit-engine`) since user-facing
/// flag spelling is this crate's concern, not the planner's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum EventArg {
    #[value(name = "push")]
    Push,
    #[value(name = "pull_request")]
    PullRequest,
    #[value(name = "workflow_dispatch")]
    WorkflowDispatch,
}

impl From<EventArg> for greenlit_engine::EventKind {
    fn from(value: EventArg) -> Self {
        match value {
            EventArg::Push => greenlit_engine::EventKind::Push,
            EventArg::PullRequest => greenlit_engine::EventKind::PullRequest,
            EventArg::WorkflowDispatch => greenlit_engine::EventKind::WorkflowDispatch,
        }
    }
}

/// Parses one `--var` occurrence's `KEY=VALUE` text.
fn parse_key_val(raw: &str) -> Result<(String, String), String> {
    match raw.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_string(), value.to_string())),
        _ => Err(format!(
            "invalid KEY=VALUE argument '{raw}': expected a non-empty KEY"
        )),
    }
}

/// Parses and validates one GitHub configuration-variable override.
fn parse_var(raw: &str) -> Result<(String, String), String> {
    let (key, value) = parse_key_val(raw)?;
    crate::vars::validate_name(&key).map_err(|reason| {
        format!(
            "invalid --var name '{key}': {reason}\n  fix: rename it to use only letters, digits, and underscores, starting with a letter or underscore and not GITHUB_"
        )
    })?;
    Ok((key, value))
}

/// Parses and validates one secret override. `GITHUB_TOKEN` is deliberately
/// accepted here even though it starts with the reserved `GITHUB_` prefix
/// (which every other name is rejected for) — it is the one name
/// `crate::secrets`' ordinary chain excludes entirely in favor of
/// `crate::auth::resolve_github_token`, and a local `-s GITHUB_TOKEN=...`
/// override is that resolution's own highest-priority source
/// (`crate::run_cmd`), not a name a repository could ever store as a real
/// secret.
fn parse_secret(raw: &str) -> Result<(String, String), String> {
    let (key, value) = parse_key_val(raw)?;
    if key.eq_ignore_ascii_case(crate::secrets::GITHUB_TOKEN_NAME) {
        return Ok((key, value));
    }
    crate::secrets::validate_name(&key).map_err(|reason| {
        format!(
            "invalid -s/--secret name '{key}': {reason}\n  fix: rename it to use only letters, digits, and underscores, starting with a letter or underscore and not GITHUB_"
        )
    })?;
    Ok((key, value))
}
