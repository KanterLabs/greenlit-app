//! Container runtime for Greenlit: the [`ContainerEngine`] trait, its
//! bollard-backed Docker implementation, three-state engine detection, and
//! host-platform validation.
//!
//! `greenlit-v0-spec.md` ("Tech"): "All engine access goes through one Rust
//! trait (Docker-API client behind it), so future platforms and architectures
//! are ports, not rewrites." Base-image construction and overlayfs workspace
//! isolation are the remaining Phase 2 runtime concerns and land in sibling
//! modules built by their own task groups.
//!
//! # Layout
//!
//! - [`engine`] — the [`ContainerEngine`] port and its request/output types.
//! - [`docker`] — the bollard/Docker backend ([`DockerEngine`]). No shelling
//!   out to the `docker` binary, ever.
//! - [`mod@detect`] — three-state engine detection behind the injected
//!   [`EngineProber`]; failing states carry a fix action, never a raw error.
//! - [`platform`] — Linux x86_64 host validation and the runner-label →
//!   base-image map.
//! - [`image`] — the convergent base image: assembly, content-addressed
//!   tagging, and build-on-first-use, with the embedded `greenlit-init` helper.
//! - [`isolation`] — the overlay-isolation container spec (read-only repo bind
//!   + helper entrypoint).
//! - [`writeback`] — the host-side `--write-back` flow: export, confirm, apply.
//! - [`executor`] — the plan executor: one container per job, one exec per
//!   step, sequential DAG order, driving the engine's execution semantics and
//!   streaming live logs; job-container option validation lives in
//!   [`executor::container`].
#![forbid(unsafe_code)]

pub mod detect;
pub mod docker;
pub mod engine;
pub mod error;
pub mod executor;
pub mod image;
pub mod isolation;
pub mod platform;
pub mod progress;
pub mod writeback;

pub use detect::{Endpoint, EngineFix, EngineProber, EngineState, SystemProber, detect};
pub use docker::DockerEngine;
pub use engine::{
    BindMount, BuildSpec, CommitSpec, ContainerEngine, ContainerSpec, ExecOutput, ExecOutputSink,
    ExecSpec, RegistryAuth, SinkNull,
};
pub use error::{Operation, RuntimeError};
pub use executor::{
    ExecError, JobReport, ReadinessConfig, RunConfig, RunReport, StepReport,
    actions::ActionRuntimeConfig, actions::node_runtime::HttpRuntimeBundleFetcher,
    container::ContainerRejection, reject_uses_steps, run_plan,
};
pub use image::{BaseImagePlan, ImageError, ensure_base_image, init_binary, plan_base_image};
pub use isolation::{IsolationStrategy, isolation_container_spec};
pub use platform::{UbuntuRelease, UnsupportedHost, resolve_base_image, validate_host};
pub use progress::{ProgressEvent, ProgressNull, ProgressSink, WorkspaceProgress};
pub use writeback::{
    Change, ChangeKind, Confirm, InteractiveConfirm, NoInputConflict, OverlayDiff, WriteBackError,
    WriteBackOutcome, run_write_back, validate_request,
};
