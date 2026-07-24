//! Rendering the resolved plan for a human (default, stdout) and the
//! lint/timing side-channel output that always goes to stderr, never stdout
//! (`PHASE-1-engine-core.md`: "plan --json writes exactly one stable plan
//! document to stdout... timing/run metadata is excluded from the plan
//! schema").

pub(crate) mod diagnostics;
pub(crate) mod progress;
pub(crate) mod terminal;
pub(crate) mod tree;
