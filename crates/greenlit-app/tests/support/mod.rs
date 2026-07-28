//! Shared integration-test harness: spawns the compiled `litci` binary into
//! an isolated temp working directory and `$HOME`.
//!
//! Lives at `tests/support/mod.rs` (not `tests/support.rs`) so Cargo does
//! not treat it as its own test binary; each real test file pulls it in
//! with `mod support;`. Per `TESTING.md` ("Fixtures, builders, and helpers
//! stay too simple to need tests") this module has no tests of its own.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::{ffi::OsString, path::Path};

pub mod fake_github;

/// An isolated `litci` invocation environment: its own working directory
/// (where fixture workflows/`.litci/` live) and its own `$HOME` (so
/// `~/.litci/metrics/runs.ndjson` never touches the real developer
/// machine).
pub struct Sandbox {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
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
        self.write_bytes(relative, contents.as_bytes())
    }

    /// Writes raw bytes under the sandbox working directory. This is used
    /// only for malformed-input boundaries that a Rust string cannot
    /// represent, such as a non-UTF-8 dotenv file.
    pub fn write_bytes(&self, relative: &str, contents: &[u8]) -> PathBuf {
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

    /// The sandbox's isolated `$HOME` directory.
    pub fn home(&self) -> &Path {
        self.home.path()
    }

    /// Writes `contents` to `relative` under the sandbox's isolated `$HOME`.
    /// Credential success tests use the production keyring capability target;
    /// this helper never represents a credential-storage backend.
    pub fn write_home(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.home.path().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs under sandbox HOME");
        }
        std::fs::write(&path, contents).expect("write sandbox HOME file");
        path
    }

    /// The sandbox repository working-directory path, for integration
    /// boundaries that need to populate it through an external tool such as
    /// `git clone` rather than [`Sandbox::write`].
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Initializes the sandbox's working directory as a git repository with
    /// one empty commit on `main` -- the minimum
    /// [`greenlit_engine::git::collect_git_context`] needs to build a
    /// synthetic event (a repo, a resolvable branch, and a `HEAD` commit).
    pub fn init_git(&self) {
        self.init_git_on("main");
    }

    /// Initializes the sandbox repository on the requested branch.
    pub fn init_git_on(&self, branch: &str) {
        self.git(&["init", "-q", "-b", branch]);
        self.git(&["config", "user.email", "litci-tests@example.com"]);
        self.git(&["config", "user.name", "litci tests"]);
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    }

    /// Runs a git command against the sandbox repository.
    pub fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(self.dir.path())
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
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
        self.run_from_with_env(".", args, extra_env)
    }

    /// Runs the custom-cfg compiled CLI against the real persistent Linux
    /// keyring under one unique test description. Missing keyring support is
    /// an ordinary command failure; this helper never substitutes storage.
    pub fn run_with_credential_keyring(
        &self,
        args: &[&str],
        description: &str,
        extra_env: &[(&str, &str)],
    ) -> Output {
        let mut cmd = self.command_from(Path::new("."), args);
        configure_credential_keyring(&mut cmd, description, extra_env);
        cmd.output()
            .expect("spawn litci with persistent-keyring capability")
    }

    /// Runs `litci` with raw operating-system environment strings. Linux
    /// permits non-UTF-8 values, so malformed process input needs this path
    /// rather than the ordinary UTF-8 convenience wrapper above.
    pub fn run_with_os_env(&self, args: &[&str], extra_env: &[(OsString, OsString)]) -> Output {
        let mut cmd = self.command_from(Path::new("."), args);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.output().expect("spawn litci")
    }

    /// Runs `litci` with `stdin_input` piped to its stdin (a real pipe, not
    /// a terminal — `litci auth --pat`'s piped-token path,
    /// `crate::auth::pat::prompt_for_pat`, is written specifically for this
    /// non-interactive shape).
    pub fn run_with_stdin(
        &self,
        args: &[&str],
        extra_env: &[(&str, &str)],
        stdin_input: &str,
    ) -> Output {
        use std::io::Write;
        use std::process::Stdio;

        let mut cmd = self.command_from(Path::new("."), args);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn litci");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin_input.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait for litci")
    }

    /// The piped-stdin variant of [`Self::run_with_credential_keyring`].
    pub fn run_with_credential_keyring_stdin(
        &self,
        args: &[&str],
        description: &str,
        extra_env: &[(&str, &str)],
        stdin_input: &str,
    ) -> Output {
        use std::io::Write;
        use std::process::Stdio;

        let mut cmd = self.command_from(Path::new("."), args);
        configure_credential_keyring(&mut cmd, description, extra_env);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn litci with persistent-keyring capability");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin_input.as_bytes())
            .expect("write credential input");
        child.wait_with_output().expect("wait for litci")
    }

    /// Runs `litci` from a directory below the sandbox repository root.
    pub fn run_from(&self, relative_cwd: &str, args: &[&str]) -> Output {
        self.run_from_with_env(relative_cwd, args, &[])
    }

    fn run_from_with_env(
        &self,
        relative_cwd: &str,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Output {
        let mut cmd = self.command_from(Path::new(relative_cwd), args);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.output().expect("spawn litci")
    }

    fn command_from(&self, relative_cwd: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(litci_bin());
        let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
        cmd.args(args)
            .current_dir(self.dir.path().join(relative_cwd))
            // A developer/CI shell may itself define names referenced by a
            // workflow. Inheriting that ambient environment would make the
            // CLI > process-env > dotenv contract nondeterministic, so the
            // harness starts from a minimal process boundary and adds only
            // values each test explicitly supplies.
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            // The kernel keyring `crate::auth::token_store` uses is scoped to
            // the real test-runner process's UID, not to the sandboxed
            // `$HOME` above. Ordinary portable cases therefore disable
            // credential storage entirely; only the required-feature
            // credential target opts into a unique description on the real
            // persistent ring and performs exact-key cleanup.
            .env("LITCI_TEST_NO_KEYRING", "1")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        cmd
    }
}

fn configure_credential_keyring(
    command: &mut Command,
    description: &str,
    extra_env: &[(&str, &str)],
) {
    command
        .env_remove("LITCI_TEST_NO_KEYRING")
        .env("LITCI_TEST_KEYRING_DESCRIPTION", description);
    for (key, value) in extra_env {
        command.env(key, value);
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

/// Proves a completed live run left no managed Docker container, network, or
/// volume behind. Listing failures are test failures too: a capability gate
/// cannot claim cleanup when its external observer is unavailable.
pub fn assert_run_resources_removed(run: &Path) {
    let run_id = run
        .file_name()
        .and_then(|name| name.to_str())
        .expect("run directory has a UTF-8 identity");
    let label_filter = format!("label=greenlit.run={run_id}");
    assert_docker_listing_empty(
        &["ps", "--all", "--quiet", "--filter", &label_filter],
        "container",
        run_id,
    );
    let name_filter = format!("name=greenlit-run-{run_id}");
    assert_docker_listing_empty(
        &["network", "ls", "--quiet", "--filter", &name_filter],
        "network",
        run_id,
    );
    assert_docker_listing_empty(
        &["volume", "ls", "--quiet", "--filter", &name_filter],
        "volume",
        run_id,
    );
}

fn assert_docker_listing_empty(args: &[&str], resource: &str, run_id: &str) {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("spawn Docker cleanup observer");
    assert!(
        output.status.success(),
        "could not verify {resource} cleanup for run {run_id}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.iter().all(u8::is_ascii_whitespace),
        "run {run_id} retained a managed Docker {resource}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
