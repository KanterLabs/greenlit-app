// `greenlit-init` is the one workspace crate permitted `unsafe`, because it
// wraps the kernel `mount(2)` overlay call. `unsafe` is denied crate-wide and
// re-enabled only inside the single documented `mount` module (its top-level
// `#![allow(unsafe_code)]`), which carries the safety invariants. Every other
// module — and every other crate — stays `unsafe`-free.
#![deny(unsafe_code)]
#![warn(missing_docs)]

//! `greenlit-init`: the private, container-only workspace init helper.
//!
//! Greenlit mounts a repository into a workflow container **read-only** at the
//! Docker level, then runs this binary as the container entrypoint. It layers
//! a throwaway writable overlay over that read-only checkout and `exec(2)`s the
//! job command against the merged, writable view — so a workflow can write, and
//! even `rm -rf` its workspace, while every write lands in a discarded upper
//! layer and the host working tree is never touched (`greenlit-v0-spec.md`,
//! "Security model"). Where unprivileged overlayfs is unavailable it falls back
//! to copying the checkout into a container-local directory and prints a
//! machine-detectable marker ([`FALLBACK_MARKER`]) to stderr.
//!
//! This crate is never installed as a host command and is never published
//! (`publish = false`). It is compiled to a single static binary, embedded in
//! `litci`, and extracted only into a base-image build context.
//!
//! # Runtime contract
//!
//! Invoked as the container entrypoint with an explicit layout and the job
//! command after a `--` separator:
//!
//! ```text
//! greenlit-init --lower <ro-repo> --upper <tmpdir> --workspace <path> \
//!     [--strategy auto|overlay|copy-in] \
//!     [--file-copy auto|reflink|bounded-stream] -- <program> [args...]
//! ```
//!
//! - `--lower` — the Docker-level read-only repo bind (the overlay lower layer).
//! - `--upper` — a container-local writable directory; the helper creates
//!   `upper/` and `work/` under it for the overlay upper and work dirs.
//! - `--workspace` — where the merged, writable checkout is exposed (the job's
//!   `GITHUB_WORKSPACE`).
//! - `--strategy` — `auto` (default: overlay, fall back to copy-in), `overlay`
//!   (require overlay; fail if unavailable), or `copy-in` (skip overlay
//!   entirely). CI uses the explicit forms to exercise *both* isolation paths,
//!   per `TESTING.md`'s isolation-path coverage rule.
//! - `--file-copy` — `auto` (default: reflink first, bounded stream fallback),
//!   `reflink` (require a successful kernel `FICLONE`), or `bounded-stream`
//!   (bypass reflink). Normal runs use `auto`; capability gates use the forced
//!   forms to prove each real path.
//!
//! [`run`] establishes the isolation, then `exec(2)`s the command in
//! `--workspace`, replacing this process so the job command succeeds the
//! entrypoint directly. It returns only on failure.
//!
//! # Embedding contract
//!
//! `greenlit-runtime` owns the `build.rs`/`include_bytes!` wiring that bakes
//! this binary into `litci`. Its package carries this canonical source tree
//! and a standalone manifest so registry consumers never depend on an
//! unpublished sibling crate. The plumbing honours this contract:
//!
//! 1. **Build** a static, musl-linked binary with the dedicated profile:
//!
//!    ```text
//!    cargo build -p greenlit-init --profile init \
//!        --target x86_64-unknown-linux-musl
//!    ```
//!
//!    The artifact lands at `target/x86_64-unknown-linux-musl/init/greenlit-init`
//!    (relative to `CARGO_TARGET_DIR`). The `init` profile (defined in the
//!    workspace-root `Cargo.toml`) strips symbols, enables LTO, and aborts on
//!    panic to keep the embedded blob small; the musl target links statically
//!    by default, so the binary carries no shared-object dependencies and runs
//!    in a minimal image context.
//! 2. **Embed** those bytes into `litci` at compile time as an opaque blob.
//!    Nothing else in the workspace links this crate.
//! 3. **Extract** them, during base-image assembly only, to a fixed in-image
//!    path (mode `0755`) used as the container entrypoint. The bytes are never
//!    written onto the host filesystem outside a build context.

mod cli;
mod copy_in;
mod error;
mod mount;
mod run;
mod status;
mod strategy;

pub use cli::{Args, CliError, FileCopyPolicy, StrategyPref};
pub use copy_in::COPY_REPORT_MARKER;
pub use error::InitError;
pub use run::run;
pub use strategy::FALLBACK_MARKER;
