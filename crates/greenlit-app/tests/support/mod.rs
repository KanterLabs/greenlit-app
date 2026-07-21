//! Shared integration-test harness: spawns the compiled `litci` binary into
//! an isolated temp working directory and `$HOME`.
//!
//! Lives at `tests/support/mod.rs` (not `tests/support.rs`) so Cargo does
//! not treat it as its own test binary; each real test file pulls it in
//! with `mod support;`. Per `TESTING.md` ("Fixtures, builders, and helpers
//! stay too simple to need tests") this module has no tests of its own.

use std::path::PathBuf;
use std::process::{Command, Output};

/// An isolated `litci` invocation environment: its own working directory
/// (where fixture workflows/`.litci/` live) and its own `$HOME` (so
/// `~/.litci/metrics/runs.ndjson` never touches the real developer
/// machine).
pub struct Sandbox {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Sandbox {
    /// Builds a fresh sandbox with an empty working directory and `$HOME`.
    pub fn new() -> Self {
        Sandbox {
            dir: tempfile::tempdir().expect("tempdir for sandbox cwd"),
            home: tempfile::tempdir().expect("tempdir for sandbox HOME"),
        }
    }

    /// Writes `contents` to `relative` (under the sandbox's working
    /// directory), creating parent directories as needed, and returns the
    /// full path. Callers embed committed `fixtures/*.yml` content with
    /// `include_str!` rather than reading the repo's `fixtures/` directory
    /// at test-run time, so this is the one write path every test uses.
    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.dir.path().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs for sandbox file");
        }
        std::fs::write(&path, contents).expect("write sandbox file");
        path
    }

    /// The NDJSON metrics file this sandbox's `$HOME` resolves to.
    pub fn metrics_file(&self) -> PathBuf {
        greenlit_metrics::MetricsStore::default_path_under(self.home.path())
    }

    /// Initializes the sandbox's working directory as a git repository with
    /// one empty commit on `main` -- the minimum
    /// [`greenlit_engine::git::collect_git_context`] needs to build a
    /// synthetic event (a repo, a resolvable branch, and a `HEAD` commit).
    pub fn init_git(&self) {
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(self.dir.path())
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "litci-tests@example.com"]);
        git(&["config", "user.name", "litci tests"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    }

    /// Runs `litci` with `args`, the sandbox as both cwd and `$HOME`, and no
    /// extra environment variables.
    pub fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    /// Runs `litci` with `args` plus additional process environment
    /// variables (used to exercise the "process environment" leg of the
    /// `vars.*` resolution chain).
    pub fn run_with_env(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(litci_bin());
        cmd.args(args)
            .current_dir(self.dir.path())
            .env("HOME", self.home.path());
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.output().expect("spawn litci")
    }
}

fn litci_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_litci"))
}

pub fn stdout_text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("litci stdout must be valid UTF-8")
}

pub fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("litci stderr must be valid UTF-8")
}
