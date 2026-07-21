//! YAML parsing internals: GitHub's scalar-typing rules ([`scalar`]) and the
//! span-preserving raw tree builder ([`raw`]) that `crate::parse` walks into
//! the typed workflow model.
//!
//! Nothing in this module is public outside the crate — `RawNode` and
//! friends are an implementation detail of `crate::parse`, not part of
//! `greenlit-workflow`'s API.

pub(crate) mod raw;
pub(crate) mod scalar;
pub(crate) mod tag;
