//! The typed workflow model: every public type `greenlit-engine` (and, via
//! it, the `litci` CLI) builds on.

pub mod job;
pub mod step;
pub mod trigger;
pub mod value;
pub mod workflow;
