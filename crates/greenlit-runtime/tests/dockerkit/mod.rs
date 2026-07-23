//! Shared helpers for the real-daemon integration tests.
//!
//! `PHASE-2-execution.md` exit criteria 1–3 need a live engine. These tests run
//! whenever Docker is reachable (it is in this project's environment) and print
//! a clear notice, rather than silently passing, when it is not. The engine is a
//! true external, so it is used real here — not faked (`TESTING.md`).
//!
//! Not every symbol is used by every test binary that includes this module, so
//! the odd unused helper is expected.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use greenlit_runtime::DockerEngine;
use greenlit_runtime::detect::Endpoint;
use greenlit_runtime::engine::{ContainerEngine, ExecOutputSink};

/// Connect to the local Docker daemon if one is reachable.
///
/// Returns `None` (after a stderr notice) when no daemon answers, so a
/// daemon-less CI run degrades to a no-op rather than a hard failure.
pub async fn engine_if_reachable() -> Option<DockerEngine> {
    let engine = DockerEngine::connect(&Endpoint::DockerSocket).ok()?;
    // A cheap round-trip proves the daemon actually answers, not merely that a
    // client was constructed.
    match engine
        .image_exists("greenlit/probe:definitely-absent")
        .await
    {
        Ok(_) => Some(engine),
        Err(_) => None,
    }
}

/// Emit the standard "skipped — no daemon" notice for a test.
pub fn notice_no_daemon(test: &str) {
    eprintln!("{test}: no Docker daemon reachable; skipping the real-daemon checks");
}

/// A unique-per-run container/name suffix.
pub fn unique_suffix() -> String {
    format!("{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')', ' '], "")
}

/// An [`ExecOutputSink`] that keeps stdout and stderr in separate buffers.
#[derive(Default)]
pub struct CollectSink {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CollectSink {
    /// The collected stdout as a lossy UTF-8 string.
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// The collected stderr as a lossy UTF-8 string.
    pub fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

impl ExecOutputSink for CollectSink {
    fn on_stdout(&mut self, chunk: &[u8]) {
        self.stdout.extend_from_slice(chunk);
    }
    fn on_stderr(&mut self, chunk: &[u8]) {
        self.stderr.extend_from_slice(chunk);
    }
}

/// Seed a throwaway host "repository" with a canary file and a nested file.
pub fn seed_repo(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("greenlit-repo-{tag}-{}", unique_suffix()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("mk repo");
    std::fs::write(root.join("canary.txt"), b"canary").expect("write canary");
    std::fs::write(root.join("src/lib.txt"), b"nested").expect("write nested");
    root
}

/// A deterministic fingerprint of a directory tree (relative paths + file
/// bytes), for "host tree unchanged" assertions.
pub fn tree_fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").path())
            .collect();
        entries.sort();
        for path in entries {
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.push((format!("{rel}/"), Vec::new()));
                stack.push(path);
            } else {
                out.push((rel, std::fs::read(&path).expect("read file")));
            }
        }
    }
    out.sort();
    out
}
