use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

use crate::support::Sandbox;

pub(super) const RUN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
enum LaunchBoundary {
    Direct,
    RestrictiveUmask,
    AddressSpaceLimit { kibibytes: u64 },
}

pub(crate) struct RunningLitci {
    child: Option<Child>,
    stdout_prefix: Vec<u8>,
}

impl RunningLitci {
    pub(crate) fn spawn(sandbox: &Sandbox, workflow: &str) -> Self {
        Self::spawn_inner(sandbox, workflow, &[])
    }

    pub(crate) fn spawn_with_env(
        sandbox: &Sandbox,
        workflow: &str,
        extra_env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_inner(sandbox, workflow, extra_env)
    }

    pub(crate) fn spawn_under_restrictive_umask(sandbox: &Sandbox, workflow: &str) -> Self {
        Self::spawn_inner_with_options(
            sandbox,
            workflow,
            Stdio::piped(),
            LaunchBoundary::RestrictiveUmask,
            &[],
        )
    }

    pub(crate) fn spawn_with_address_space_limit(
        sandbox: &Sandbox,
        workflow: &str,
        kibibytes: u64,
    ) -> Self {
        Self::spawn_inner_with_options(
            sandbox,
            workflow,
            Stdio::piped(),
            LaunchBoundary::AddressSpaceLimit { kibibytes },
            &[],
        )
    }

    fn spawn_inner(sandbox: &Sandbox, workflow: &str, extra_env: &[(&str, &str)]) -> Self {
        Self::spawn_inner_with_options(
            sandbox,
            workflow,
            Stdio::piped(),
            LaunchBoundary::Direct,
            extra_env,
        )
    }

    fn spawn_inner_with_options(
        sandbox: &Sandbox,
        workflow: &str,
        stdout: Stdio,
        launch_boundary: LaunchBoundary,
        extra_env: &[(&str, &str)],
    ) -> Self {
        sandbox.write(".github/workflows/retained-secret.yml", workflow);
        sandbox.init_git();
        let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
        let mut command = match launch_boundary {
            LaunchBoundary::Direct => Command::new(env!("CARGO_BIN_EXE_litci")),
            LaunchBoundary::RestrictiveUmask => {
                shell_wrapped_command("umask 0777; exec \"$@\"", "litci-restrictive-umask")
            }
            LaunchBoundary::AddressSpaceLimit { kibibytes } => shell_wrapped_command(
                &format!("ulimit -v {kibibytes} || exit 125; exec \"$@\""),
                "litci-address-space-limit",
            ),
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

    pub(crate) fn signal_interrupt(&self) {
        let raw = self.child.as_ref().expect("running litci").id();
        let pid = Pid::from_raw(raw.try_into().expect("litci pid fits RawPid"))
            .expect("litci pid is nonzero");
        kill_process(pid, Signal::INT).expect("send SIGINT to litci");
    }

    pub(crate) fn signal_kill(&self) {
        let raw = self.child.as_ref().expect("running litci").id();
        let pid = Pid::from_raw(raw.try_into().expect("litci pid fits RawPid"))
            .expect("litci pid is nonzero");
        kill_process(pid, Signal::KILL).expect("send SIGKILL to litci");
    }

    pub(crate) fn close_stdout_after_line(&mut self, expected: &[u8]) {
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

    pub(crate) fn exited_output(&mut self) -> Option<Output> {
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

    pub(crate) fn terminate_and_collect(&mut self) -> Output {
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

    pub(crate) fn finish(mut self) -> Output {
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

fn shell_wrapped_command(script: &str, argument_zero: &str) -> Command {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .arg(argument_zero)
        .arg(env!("CARGO_BIN_EXE_litci"));
    command
}

impl Drop for RunningLitci {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
