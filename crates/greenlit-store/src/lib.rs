//! Local stores behind the workflow-facing shim: `actions/cache`, artifacts,
//! and the persistent toolcache.
//!
//! `PHASE-4-environment.md` ("Cache and artifacts"): the cache and artifact
//! actions reach their service over HTTP, addressed by `ACTIONS_CACHE_URL`,
//! `ACTIONS_RESULTS_URL`, and `ACTIONS_RUNTIME_TOKEN`. Greenlit answers those
//! calls locally rather than teaching every action a new protocol, so an
//! unmodified `actions/cache@v4` behaves exactly as it does on a hosted
//! runner while its bytes stay on this machine.
//!
//! This crate owns the durable side of that: what is stored, where, and which
//! entry a lookup selects. It performs no I/O beyond its own root directory
//! and opens no sockets — `greenlit-runtime` mounts the shim onto the job
//! network and hands it one of these stores.
//!
//! # Crate boundary
//!
//! * [`cache`] — the `actions/cache` backing store and its selection rule.
//! * [`server`] — the workflow-facing HTTP shim those actions talk to.
//!
//! Every store resolves its root the same way (`~/.litci/<name>`, overridable
//! by constructing with an explicit path in tests), mirroring
//! `greenlit_actions::ActionStore` and `greenlit_metrics::MetricsStore`.

#![forbid(unsafe_code)]

pub mod cache;
pub mod error;
pub mod server;

pub use cache::{CacheStore, Restored};
pub use error::StoreError;
pub use server::{Bound, Shim, ShimState, bind};
