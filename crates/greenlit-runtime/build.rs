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
//! The helper is built with an explicitly configured `init` profile for the
//! musl target (`embedded-init/src/lib.rs` embedding contract): a small,
//! static, dependency-free binary that runs in a minimal image context. The
//! explicit Cargo configuration is required when `greenlit-runtime` is
//! verified from its standalone package, where the workspace profile table is
//! unavailable. The sub-build uses a private target directory under `OUT_DIR`
//! so it never contends with the outer build's target-dir lock.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The musl target the helper is built for (static, per the embedding contract).
const INIT_TARGET: &str = "x86_64-unknown-linux-musl";
/// The workspace profile that strips + LTO-shrinks the helper.
const INIT_PROFILE: &str = "init";
/// Complete standalone definition of the workspace's `[profile.init]`.
///
/// Cargo package verification builds this script outside the workspace, so
/// every inherited and overridden setting must travel with the nested command.
const INIT_PROFILE_CONFIG: &[&str] = &[
    r#"profile.init.inherits="release""#,
    r#"profile.init.opt-level="z""#,
    "profile.init.lto=true",
    "profile.init.codegen-units=1",
    r#"profile.init.panic="abort""#,
    "profile.init.strip=true",
];
/// Canonical helper source files packaged inside `greenlit-runtime`.
const INIT_SOURCE_FILES: &[&str] = &[
    "cli.rs",
    "copy_in.rs",
    "error.rs",
    "lib.rs",
    "main.rs",
    "mount.rs",
    "run.rs",
    "status.rs",
    "strategy.rs",
];
/// Standalone helper manifest stored with a non-Cargo name so Cargo does not
/// treat the packaged source as a nested package and omit it.
const INIT_MANIFEST: &str = include_str!("embedded-init/Cargo.init.toml");

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

    let packaged_init = manifest_dir.join("embedded-init");
    let packaged_sources = packaged_init.join("src");
    let packaged_manifest = packaged_init.join("Cargo.init.toml");
    let packaged_lock = packaged_init.join("Cargo.init.lock");
    rerun_if_changed(&packaged_sources);
    rerun_if_changed(&packaged_manifest);
    rerun_if_changed(&packaged_lock);

    // A private target directory so the nested cargo never blocks on the outer
    // build's `target/` lock.
    let sub_target = out_dir.join("init-target");
    let init_dir = out_dir.join("init-crate");
    let init_sources = init_dir.join("src");
    std::fs::create_dir_all(&init_sources).map_err(|error| {
        format!(
            "failed to create the packaged greenlit-init build tree at {}: {error}",
            init_sources.display()
        )
    })?;
    for name in INIT_SOURCE_FILES {
        let source = packaged_sources.join(name);
        let destination = init_sources.join(name);
        std::fs::copy(&source, &destination).map_err(|error| {
            format!(
                "failed to stage packaged greenlit-init source {} at {}: {error}",
                source.display(),
                destination.display()
            )
        })?;
    }
    let init_manifest = init_dir.join("Cargo.toml");
    std::fs::write(&init_manifest, INIT_MANIFEST).map_err(|error| {
        format!(
            "failed to stage the packaged greenlit-init manifest at {}: {error}",
            init_manifest.display()
        )
    })?;
    let init_lock = init_dir.join("Cargo.lock");
    std::fs::copy(&packaged_lock, &init_lock).map_err(|error| {
        format!(
            "failed to stage the packaged greenlit-init lock from {} at {}: {error}",
            packaged_lock.display(),
            init_lock.display()
        )
    })?;

    let mut command = Command::new(&cargo);
    command.arg("build").arg("--locked");
    for config in INIT_PROFILE_CONFIG {
        command.arg("--config").arg(config);
    }
    let status = command
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
