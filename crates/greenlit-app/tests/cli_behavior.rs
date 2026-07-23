//! CLI integration contracts outside the named `matrix-needs` fixture.

pub mod support;

#[path = "cli_behavior/common.rs"]
mod common;
#[path = "cli_behavior/discovery.rs"]
mod discovery;
#[path = "cli_behavior/dotenv.rs"]
mod dotenv;
#[path = "cli_behavior/filters.rs"]
mod filters;
#[path = "cli_behavior/git_context.rs"]
mod git_context;
#[path = "cli_behavior/repository.rs"]
mod repository;
