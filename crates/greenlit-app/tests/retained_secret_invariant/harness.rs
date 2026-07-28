use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

use crate::support::Sandbox;

const RUN_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) struct RunningLitci {
    child: Option<Child>,
}

impl RunningLitci {
    pub(super) fn spawn(sandbox: &Sandbox, workflow: &str) -> Self {
        Self::spawn_inner(sandbox, workflow, None)
    }

    pub(super) fn spawn_with_secret(
        sandbox: &Sandbox,
        workflow: &str,
        name: &str,
        value: &str,
    ) -> Self {
        Self::spawn_inner(sandbox, workflow, Some((name, value)))
    }

    fn spawn_inner(sandbox: &Sandbox, workflow: &str, secret: Option<(&str, &str)>) -> Self {
        sandbox.write(".github/workflows/retained-secret.yml", workflow);
        sandbox.init_git();
        let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_litci"));
        command
            .args([
                "run",
                "--no-daemon",
                "--no-input",
                "--allow-degraded",
                "--log-mode",
                "full",
                "-W",
                ".github/workflows/retained-secret.yml",
            ])
            .current_dir(sandbox.root())
            .env_clear()
            .env("PATH", path)
            .env("HOME", sandbox.home())
            .env("XDG_CONFIG_HOME", sandbox.home().join(".config"))
            .env("LITCI_TEST_NO_KEYRING", "1")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        if let Some((name, value)) = secret {
            command.arg("--secret").arg(format!("{name}={value}"));
        }
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn compiled litci");
        RunningLitci { child: Some(child) }
    }

    pub(super) fn signal_interrupt(&self) {
        let raw = self.child.as_ref().expect("running litci").id();
        let pid = Pid::from_raw(raw.try_into().expect("litci pid fits RawPid"))
            .expect("litci pid is nonzero");
        kill_process(pid, Signal::INT).expect("send SIGINT to litci");
    }

    pub(super) fn close_stdout(&mut self) {
        let stdout = self
            .child
            .as_mut()
            .expect("running litci")
            .stdout
            .take()
            .expect("piped litci stdout");
        drop(stdout);
    }

    pub(super) fn finish(mut self) -> Output {
        let mut child = self.child.take().expect("running litci");
        let deadline = Instant::now() + RUN_TIMEOUT;
        loop {
            if child.try_wait().expect("poll litci").is_some() {
                return child.wait_with_output().expect("collect litci output");
            }
            if Instant::now() >= deadline {
                child.kill().expect("terminate timed-out litci");
                let _ = child.wait();
                panic!("litci did not terminate within the invariant-test deadline");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for RunningLitci {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(super) struct NetworkGuard {
    name: Option<String>,
}

impl NetworkGuard {
    pub(super) fn create(name: String) -> Self {
        let output = docker(["network", "create", &name]);
        assert!(
            output.status.success(),
            "could not create the preparation-failure network collision"
        );
        NetworkGuard { name: Some(name) }
    }

    pub(super) fn cleanup(mut self) {
        let name = self.name.take().expect("live network guard");
        let output = docker(["network", "rm", &name]);
        assert!(
            output.status.success(),
            "could not remove the preparation-failure network collision"
        );
        assert!(
            !network_exists(&name),
            "preparation-failure network collision remained after cleanup"
        );
    }
}

impl Drop for NetworkGuard {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            let _ = docker(["network", "rm", &name]);
        }
    }
}

pub(super) struct ContainerGuard {
    id: Option<String>,
}

impl ContainerGuard {
    pub(super) fn new(id: String) -> Self {
        ContainerGuard { id: Some(id) }
    }

    pub(super) fn cleanup(mut self) {
        let id = self.id.take().expect("live container guard");
        if container_exists(&id) {
            let output = docker(["rm", "-f", &id]);
            assert!(
                output.status.success(),
                "could not remove the invariant workflow container"
            );
        }
        assert!(
            !container_exists(&id),
            "invariant workflow container remained after cleanup"
        );
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = docker(["rm", "-f", &id]);
        }
    }
}

pub(super) fn assert_real_docker() {
    let output = docker(["info", "--format", "{{.ServerVersion}}"]);
    assert!(
        output.status.success() && !output.stdout.is_empty(),
        "this invariant target requires an available real local Docker daemon"
    );
}

pub(super) fn observe_runtime_token(sandbox: &Sandbox) -> (String, String, String) {
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        if let Some(run_id) = current_run_id(sandbox)
            && let Some(container) = running_container(&run_id)
            && let Some(token) = token_from_step_process(&container)
        {
            return (run_id, container, token);
        }
        assert!(
            Instant::now() < deadline,
            "litci did not expose a live runtime-token step before the deadline"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

pub(super) fn wait_for_container_path(container: &str, path: &str) {
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        if docker(["exec", container, "test", "-f", path])
            .status
            .success()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "workflow did not reach its emitted marker before the deadline"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn write_container_path(container: &str, path: &str, bytes: &[u8]) {
    let mut child = Command::new("docker")
        .args([
            "exec",
            "-i",
            container,
            "sh",
            "-c",
            "umask 077; cat > \"$1\"",
            "greenlit-invariant",
            path,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("write live invariant container path");
    child
        .stdin
        .take()
        .expect("piped Docker stdin")
        .write_all(bytes)
        .expect("send invariant bytes to container");
    let output = child
        .wait_with_output()
        .expect("finish live container write");
    assert!(
        output.status.success(),
        "could not inject live invariant bytes into the workflow container"
    );
}

pub(super) fn one_run_directory(sandbox: &Sandbox) -> PathBuf {
    let mut runs = run_directories(sandbox);
    assert_eq!(runs.len(), 1, "one invocation must retain exactly one run");
    runs.pop().expect("one retained run")
}

pub(super) fn run_directories(sandbox: &Sandbox) -> Vec<PathBuf> {
    fs::read_dir(sandbox.home().join(".litci/runs"))
        .map(|entries| {
            entries
                .map(|entry| entry.expect("read retained run").path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn docker<const N: usize>(args: [&str; N]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .expect("execute Docker CLI")
}

fn current_run_id(sandbox: &Sandbox) -> Option<String> {
    let runs = sandbox.home().join(".litci/runs");
    let entries = fs::read_dir(runs).ok()?;
    entries
        .filter_map(Result::ok)
        .find(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
}

fn running_container(run_id: &str) -> Option<String> {
    let filter = format!("label=greenlit.run={run_id}");
    let output = docker(["ps", "--filter", &filter, "--format", "{{.ID}}"]);
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn token_from_step_process(container: &str) -> Option<String> {
    let script = r#"for file in /proc/[0-9]*/environ; do tr '\000' '\n' < "$file" 2>/dev/null || true; done"#;
    let output = docker(["exec", container, "sh", "-c", script]);
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("ACTIONS_RUNTIME_TOKEN="))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn container_exists(id: &str) -> bool {
    let output = docker([
        "ps",
        "--all",
        "--quiet",
        "--no-trunc",
        "--filter",
        &format!("id={id}"),
    ]);
    assert!(
        output.status.success(),
        "could not verify invariant container cleanup"
    );
    !output.stdout.is_empty()
}

fn network_exists(name: &str) -> bool {
    let output = docker(["network", "ls", "--format", "{{.Name}}"]);
    assert!(
        output.status.success(),
        "could not verify invariant network cleanup"
    );
    String::from_utf8(output.stdout)
        .expect("Docker network names are UTF-8")
        .lines()
        .any(|candidate| candidate == name)
}
