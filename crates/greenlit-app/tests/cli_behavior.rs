//! CLI integration contracts outside the named `matrix-needs` fixture.

pub mod support;

#[path = "cli_behavior/auth.rs"]
mod auth;
#[path = "cli_behavior/common.rs"]
mod common;
#[path = "cli_behavior/discovery.rs"]
mod discovery;
#[path = "cli_behavior/dogfood.rs"]
mod dogfood;
#[path = "cli_behavior/dotenv.rs"]
mod dotenv;
#[path = "cli_behavior/filters.rs"]
mod filters;
#[path = "cli_behavior/git_context.rs"]
mod git_context;
#[path = "cli_behavior/github_confirmation.rs"]
mod github_confirmation;
#[path = "cli_behavior/inspect.rs"]
mod inspect;
#[path = "cli_behavior/logs.rs"]
mod logs;
#[path = "cli_behavior/policy_modes.rs"]
mod policy_modes;
#[path = "cli_behavior/repository.rs"]
mod repository;
#[path = "cli_behavior/run_preflight.rs"]
mod run_preflight;
#[path = "cli_behavior/secrets.rs"]
mod secrets;
#[path = "cli_behavior/variables_remote.rs"]
mod variables_remote;
