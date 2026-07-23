//! Build the private `greenlit-init` helper and stage its bytes for embedding.
//!
//! `PHASE-2-execution.md` ("Base image and private init helper"): the helper is
//! compiled to a single static binary, embedded in `litci`, and extracted only
//! into the base-image build context — never installed as a host command.
//! `greenlit-runtime` owns base-image assembly, so it embeds the helper here and
//! `crate::image` bakes the bytes into the build-context tar. Because `litci`
//! links `greenlit-runtime`, the bytes travel into the one distributable host
//! binary transitively; nothing writes the helper to a host executable path.
//!
//! The helper is built with the workspace `init` profile for the musl target
//! (`crates/greenlit-init/src/lib.rs` embedding contract): a small, static,
//! dependency-free binary that runs in a minimal image context. The sub-build
//! uses a private target directory under `OUT_DIR` so it never contends with
//! the outer build's target-dir lock.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The musl target the helper is built for (static, per the embedding contract).
const INIT_TARGET: &str = "x86_64-unknown-linux-musl";
/// The workspace profile that strips + LTO-shrinks the helper.
const INIT_PROFILE: &str = "init";

fn main() {
    if let Err(message) = build_and_stage_init() {
        // A build script signals failure by exiting non-zero; do so with a
        // clear operator message rather than an unwrap panic backtrace.
        eprintln!("greenlit-runtime build script: {message}");
        std::process::exit(1);
    }
}

/// Compile `greenlit-init` and copy its binary to `$OUT_DIR/greenlit-init`.
fn build_and_stage_init() -> Result<(), String> {
    let manifest_dir =
        env_path("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set by cargo")?;
    let out_dir = env_path("OUT_DIR").ok_or("OUT_DIR is not set by cargo")?;
    let cargo = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"));

    // The init crate sits beside this one in the workspace.
    let init_dir = manifest_dir
        .parent()
        .map(|crates| crates.join("greenlit-init"))
        .ok_or("could not locate the greenlit-init crate beside greenlit-runtime")?;
    let init_manifest = init_dir.join("Cargo.toml");

    // Only re-run when the helper's sources or manifest change; otherwise the
    // staged binary in OUT_DIR is reused and the sub-build is skipped.
    rerun_if_changed(&init_dir.join("src"));
    rerun_if_changed(&init_manifest);

    // A private target directory so the nested cargo never blocks on the outer
    // build's `target/` lock.
    let sub_target = out_dir.join("init-target");

    let status = Command::new(&cargo)
        .arg("build")
        .arg("--manifest-path")
        .arg(&init_manifest)
        .arg("--profile")
        .arg(INIT_PROFILE)
        .arg("--target")
        .arg(INIT_TARGET)
        .arg("--target-dir")
        .arg(&sub_target)
        .status()
        .map_err(|e| format!("failed to launch cargo for the greenlit-init sub-build: {e}"))?;
    if !status.success() {
        return Err(format!(
            "the greenlit-init sub-build failed ({status}); ensure the `{INIT_TARGET}` target is \
             installed (`rustup target add {INIT_TARGET}`)"
        ));
    }

    let built = sub_target
        .join(INIT_TARGET)
        .join(INIT_PROFILE)
        .join("greenlit-init");
    let staged = out_dir.join("greenlit-init");
    std::fs::copy(&built, &staged).map_err(|e| {
        format!(
            "failed to stage the greenlit-init binary from {} to {}: {e}",
            built.display(),
            staged.display()
        )
    })?;
    Ok(())
}

/// Read an environment variable cargo sets, as a `PathBuf`.
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).map(PathBuf::from)
}

/// Emit a `rerun-if-changed` line for `path`.
fn rerun_if_changed(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}
