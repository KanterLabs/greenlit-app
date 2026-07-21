//! Command-line argument definitions for `litci` (clap derive types).
//!
//! Kept separate from dispatch logic so the argument *shape* -- which flags
//! exist and how each one parses -- is reviewable on its own.
//! `PHASE-1-engine-core.md` greenlit-app section: "litci plan [--json] [-e
//! EVENT] [-W path] [--var KEY=VALUE]"; `litci stats` takes no flags.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// `litci` -- run your GitHub Actions workflows locally, fast, with results
/// you can trust (`greenlit-v0-spec.md`). This Phase 1 build implements only
/// `plan` and `stats`; `run`/`auth`/`setup`/`clean` are later-phase commands
/// (`PHASE-1-engine-core.md` "Out of scope (do not build)").
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
    /// Show local invocation history and per-stage timing trends. Read-only:
    /// never appends a metrics record for its own invocation.
    Stats,
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
    #[arg(long = "var", value_name = "KEY=VALUE", value_parser = parse_key_val)]
    pub(crate) vars: Vec<(String, String)>,
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
            "invalid --var '{raw}': expected KEY=VALUE with a non-empty KEY"
        )),
    }
}
