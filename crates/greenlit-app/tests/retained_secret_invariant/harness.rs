use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

use crate::support::Sandbox;

const RUN_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) struct RunningLitci {
    child: Option<Child>,
    stdout_prefix: Vec<u8>,
}

impl RunningLitci {
    pub(super) fn spawn(sandbox: &Sandbox, workflow: &str) -> Self {
        Self::spawn_inner(sandbox, workflow, &[])
    }

    pub(super) fn spawn_with_env(
        sandbox: &Sandbox,
        workflow: &str,
        extra_env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_inner(sandbox, workflow, extra_env)
    }

    pub(super) fn spawn_under_restrictive_umask(sandbox: &Sandbox, workflow: &str) -> Self {
        Self::spawn_inner_with_options(sandbox, workflow, Stdio::piped(), true, &[])
    }

    fn spawn_inner(sandbox: &Sandbox, workflow: &str, extra_env: &[(&str, &str)]) -> Self {
        Self::spawn_inner_with_options(sandbox, workflow, Stdio::piped(), false, extra_env)
    }

    fn spawn_inner_with_options(
        sandbox: &Sandbox,
        workflow: &str,
        stdout: Stdio,
        restrictive_umask: bool,
        extra_env: &[(&str, &str)],
    ) -> Self {
        sandbox.write(".github/workflows/retained-secret.yml", workflow);
        sandbox.init_git();
        let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
        let mut command = if restrictive_umask {
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg("umask 0777; exec \"$@\"")
                .arg("litci-restrictive-umask")
                .arg(env!("CARGO_BIN_EXE_litci"));
            command
        } else {
            Command::new(env!("CARGO_BIN_EXE_litci"))
        };
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
        for (name, value) in extra_env {
            command.env(name, value);
        }
        let child = command
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn compiled litci");
        RunningLitci {
            child: Some(child),
            stdout_prefix: Vec::new(),
        }
    }

    pub(super) fn signal_interrupt(&self) {
        let raw = self.child.as_ref().expect("running litci").id();
        let pid = Pid::from_raw(raw.try_into().expect("litci pid fits RawPid"))
            .expect("litci pid is nonzero");
        kill_process(pid, Signal::INT).expect("send SIGINT to litci");
    }

    pub(super) fn signal_kill(&self) {
        let raw = self.child.as_ref().expect("running litci").id();
        let pid = Pid::from_raw(raw.try_into().expect("litci pid fits RawPid"))
            .expect("litci pid is nonzero");
        kill_process(pid, Signal::KILL).expect("send SIGKILL to litci");
    }

    pub(super) fn close_stdout_after_line(&mut self, expected: &[u8]) {
        assert!(
            self.stdout_prefix.is_empty(),
            "litci stdout reader was already consumed"
        );
        let stdout = self
            .child
            .as_mut()
            .expect("running litci")
            .stdout
            .take()
            .expect("piped litci stdout");
        let expected = expected.to_vec();
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            let mut observed = Vec::new();
            let outcome = loop {
                let mut line = Vec::new();
                match stdout.read_until(b'\n', &mut line) {
                    Ok(0) => break Err("litci stdout closed before the expected line"),
                    Ok(_) => {
                        let matched = line == expected;
                        observed.extend_from_slice(&line);
                        if matched {
                            break Ok(observed);
                        }
                    }
                    Err(_) => break Err("could not read litci stdout"),
                }
            };
            drop(stdout);
            let _ = sender.send(outcome);
        });
        match receiver.recv_timeout(RUN_TIMEOUT) {
            Ok(Ok(observed)) => {
                reader.join().expect("join litci stdout reader");
                self.stdout_prefix = observed;
            }
            Ok(Err(message)) => {
                reader.join().expect("join failed litci stdout reader");
                panic!("{message}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let output = self.terminate_and_collect();
                reader.join().expect("join timed-out litci stdout reader");
                panic!(
                    "litci did not render the expected line before the invariant-test deadline: \
                     terminated {}; stdout={} bytes, stderr={} bytes",
                    output.status,
                    output.stdout.len(),
                    output.stderr.len()
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                reader
                    .join()
                    .expect("join disconnected litci stdout reader");
                panic!("litci stdout reader exited without reporting its result");
            }
        }
    }

    fn exited_output(&mut self) -> Option<Output> {
        let child = self.child.as_mut().expect("running litci");
        child.try_wait().expect("poll litci")?;
        let output = self
            .child
            .take()
            .expect("exited litci")
            .wait_with_output()
            .expect("collect exited litci output");
        Some(self.with_stdout_prefix(output))
    }

    fn terminate_and_collect(&mut self) -> Output {
        let mut child = self.child.take().expect("running litci");
        let _ = child.kill();
        let output = child
            .wait_with_output()
            .expect("collect terminated litci output");
        self.with_stdout_prefix(output)
    }

    fn with_stdout_prefix(&mut self, mut output: Output) -> Output {
        if self.stdout_prefix.is_empty() {
            return output;
        }
        self.stdout_prefix.append(&mut output.stdout);
        output.stdout = std::mem::take(&mut self.stdout_prefix);
        output
    }

    pub(super) fn finish(mut self) -> Output {
        let mut child = self.child.take().expect("running litci");
        let deadline = Instant::now() + RUN_TIMEOUT;
        loop {
            if child.try_wait().expect("poll litci").is_some() {
                let output = child.wait_with_output().expect("collect litci output");
                return self.with_stdout_prefix(output);
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

pub(super) fn observe_running_container(
    sandbox: &Sandbox,
    running: &mut RunningLitci,
) -> (String, String) {
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        if let Some(run_id) = current_run_id(sandbox)
            && let Some(container) = running_container(&run_id)
        {
            return (run_id, container);
        }
        if let Some(output) = running.exited_output() {
            panic!(
                "litci exited before exposing a live workflow container: {}; \
                 stdout={} bytes, stderr={} bytes",
                output.status,
                output.stdout.len(),
                output.stderr.len()
            );
        }
        if Instant::now() >= deadline {
            let output = running.terminate_and_collect();
            panic!(
                "litci did not expose a live workflow container before the deadline: \
                 terminated {}; stdout={} bytes, stderr={} bytes",
                output.status,
                output.stdout.len(),
                output.stderr.len()
            );
        }
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

pub(super) fn read_container_text(container: &str, path: &str) -> String {
    wait_for_container_path(container, path);
    let output = docker(["exec", container, "cat", path]);
    assert!(
        output.status.success(),
        "could not read the runtime-generated dynamic mask"
    );
    let value = String::from_utf8(output.stdout).expect("dynamic mask is UTF-8");
    assert!(
        !value.is_empty(),
        "runtime-generated dynamic mask was unexpectedly empty"
    );
    value
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
