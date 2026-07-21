//! Walking the raw YAML tree (`crate::yaml::raw`) into the typed workflow
//! model (`crate::model`).
//!
//! Split by concern: [`workflow`] is the root mapping and orchestrates the
//! rest; [`trigger`] normalizes `on:`; [`job`] covers `jobs.<id>:`;
//! [`matrix`] covers `strategy:`/`strategy.matrix:`; [`container`] covers
//! the shape shared by `container:` and `services.<id>:`; [`step`] covers
//! `steps[]:`; [`util`] holds the generic tree-walking helpers every other
//! submodule builds on.

mod container;
mod dispatch;
mod job;
mod matrix;
mod step;
mod trigger;
mod util;
mod workflow;

pub use workflow::{parse_workflow, parse_workflow_file};
