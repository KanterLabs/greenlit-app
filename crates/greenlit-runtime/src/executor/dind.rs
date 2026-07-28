//! Docker-in-Docker: a workflow that builds images gets its own daemon, never
//! the host's.
//!
//! `greenlit-v0-spec.md` ("Security model"): "No host Docker socket, ever.
//! Workflows that build/run images get an isolated Docker-in-Docker sidecar
//! instead." `PHASE-4-environment.md:36` adds the part that makes it work
//! without surprises: **deliberately prepend a managed wrapper for the
//! `docker` command even when a custom job image already contains the CLI**,
//! and attach/wait for the sidecar *before* execing the real argv — never
//! discover a missing daemon by rerunning the user's step.
//!
//! # Shape
//!
//! * The sidecar is `docker:dind` on the job's own bridge, answering to the
//!   hostname `docker`. TLS is off because the traffic never leaves that
//!   bridge, and the netguard policy already confines it there.
//! * The wrapper lands at `/greenlit/wrappers/docker`, and that directory
//!   goes **first** on `PATH`. First is the point: `PHASE-4-environment.md`
//!   says to prepend it "even when a custom job image already contains the
//!   CLI", and a job container like `docker:27-cli` ships one at
//!   `/usr/local/bin/docker`. Appending instead means the image's own CLI
//!   wins, it talks to `unix:///var/run/docker.sock`, and the step fails
//!   with "cannot connect to the Docker daemon" — which is exactly what
//!   happened before this was split in two.
//!
//!   This is the opposite of the *provisioning* shim directory
//!   ([`SHIM_DIR`]), which goes last precisely so a real tool a workflow
//!   installs always wins. The two rules are different because the jobs are
//!   different: provisioning fills a gap, this one redirects.
//! * `/greenlit` is a reserved control root
//!   (`crate::executor::container::validate_container`), so a workflow cannot
//!   mount over the shim directory.
//!
//! # Why the sidecar is privileged
//!
//! `docker:dind` does not start otherwise — `CAP_SYS_ADMIN` alone makes it
//! exit immediately (verified against a real daemon). That is the trade the
//! spec already made: the sidecar exists so the *host* daemon is never
//! exposed, it is confined to one run's bridge, and it is removed with the
//! job. Handing a workflow the host socket, which is what `act` does, is the
//! alternative being avoided.

use std::time::{Duration, Instant};

use crate::engine::{ContainerEngine, ContainerSpec, ExecSpec, SinkNull};
use crate::executor::ExecError;
use crate::progress::ProgressSink;

/// The daemon image. Pinned to a major so a `docker build` inside a workflow
/// sees a predictable server version.
const DIND_IMAGE: &str = "docker:27-dind";

/// The hostname the wrapper points `DOCKER_HOST` at.
const DIND_HOST: &str = "docker";

/// The plain-TCP port `docker:dind` serves when TLS is disabled.
const DIND_PORT: u16 = 2375;

/// Where the managed `docker` wrapper lives. **First** on `PATH`, so it
/// intercepts the image's own CLI — see the module docs.
///
/// Both directories sit under the reserved control root, so a workflow
/// `volumes:` entry can never mount over either
/// (`crate::executor::container::validate_container`).
pub(crate) const WRAPPER_DIR: &str = "/greenlit/wrappers";

/// How long the daemon has to come up. It genuinely takes seconds — a fixed
/// sleep would be either slow or flaky, so this is a deadline on a poll.
const READY_DEADLINE: Duration = Duration::from_secs(90);

/// How often readiness is polled.
const READY_POLL: Duration = Duration::from_millis(500);

/// A started Docker-in-Docker sidecar.
pub(crate) struct Dind {
    container: String,
}

impl Dind {
    /// The sidecar's container id, for teardown.
    pub(crate) fn container(&self) -> &str {
        &self.container
    }
}

/// Starts the sidecar on `network` and waits for its daemon to answer.
///
/// # Errors
/// Returns [`ExecError`] if the sidecar cannot be pulled, created, or started,
/// or if its daemon does not answer within [`READY_DEADLINE`]. Failing here is
/// right: the alternative is letting the user's own `docker build` be the
/// thing that discovers the daemon is missing, which is exactly the
/// rerun-to-find-out behavior the brief forbids.
pub(crate) async fn start(
    engine: &dyn ContainerEngine,
    network: &str,
    run_id: &str,
    progress: &mut (dyn ProgressSink + Send),
) -> Result<Dind, ExecError> {
    if !engine.image_exists(DIND_IMAGE).await? {
        engine.pull_image(DIND_IMAGE, None, progress).await?;
    }

    let spec = ContainerSpec {
        image: DIND_IMAGE.to_string(),
        network: Some(network.to_string()),
        network_aliases: vec![DIND_HOST.to_string()],
        hostname: Some(DIND_HOST.to_string()),
        // The daemon does not start unprivileged; see the module docs.
        privileged: true,
        // Empty `DOCKER_TLS_CERTDIR` selects plain TCP. The socket is
        // reachable only from this run's bridge, which the netguard policy
        // confines, so certificate machinery would add ceremony without
        // adding a boundary.
        env: vec![("DOCKER_TLS_CERTDIR".to_string(), String::new())],
        labels: vec![
            ("greenlit.managed".to_string(), "1".to_string()),
            ("greenlit.run".to_string(), run_id.to_string()),
            ("greenlit.dind".to_string(), "1".to_string()),
        ],
        ..ContainerSpec::default()
    };

    let container = engine.create_container(&spec).await?;
    let sidecar = Dind {
        container: container.clone(),
    };
    engine.start_container(&container).await?;
    wait_ready(engine, &container).await?;
    Ok(sidecar)
}

/// Polls the sidecar until its own daemon answers.
async fn wait_ready(engine: &dyn ContainerEngine, container: &str) -> Result<(), ExecError> {
    let deadline = Instant::now() + READY_DEADLINE;
    let probe = ExecSpec {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            "docker version >/dev/null 2>&1".to_string(),
        ],
        env: Vec::new(),
        working_dir: None,
    };
    loop {
        // Asking the daemon about itself, from inside its own container, is
        // the only signal that means "ready to serve" -- the container being
        // *running* happens seconds earlier.
        if let Ok(output) = engine.exec(container, &probe, &mut SinkNull).await
            && output.exit_code == 0
        {
            return Ok(());
        }
        let state = engine.inspect_container(container).await?;
        if !state.running {
            return Err(ExecError::Infrastructure {
                message: "the Docker-in-Docker sidecar exited before its daemon was ready"
                    .to_string(),
                fix: "check that the host daemon can run a privileged container; \
                      Greenlit never mounts the host Docker socket, so a workflow that \
                      builds images needs this sidecar"
                    .to_string(),
            });
        }
        if Instant::now() >= deadline {
            return Err(ExecError::Infrastructure {
                message: format!(
                    "the Docker-in-Docker sidecar's daemon did not answer within {}s",
                    READY_DEADLINE.as_secs()
                ),
                fix: "retry; if it persists, check the host daemon's storage driver — \
                      nested overlayfs is the usual cause"
                    .to_string(),
            });
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

/// The shell command that installs the managed `docker` wrapper.
///
/// Run as an exec into the job container after boot. The wrapper resolves the
/// *real* `docker` by scanning `PATH` with the shim directory removed, so it
/// can never recurse into itself, and `exec`s it with the original argv and
/// environment intact — the step is not restarted, and nothing it did before
/// calling `docker` happens twice.
pub(crate) fn install_wrapper_script() -> String {
    // Single-quoted heredoc: nothing in the body is expanded when it is
    // written, only when the wrapper itself runs.
    format!(
        "mkdir -p {WRAPPER_DIR} && cat > {WRAPPER_DIR}/docker <<'GREENLIT_DOCKER_WRAPPER'\n\
         #!/bin/sh\n\
         # Managed by Greenlit. Points the Docker CLI at this run's isolated\n\
         # daemon; the host's socket is never mounted into a workflow.\n\
         DOCKER_HOST=tcp://{DIND_HOST}:{DIND_PORT}\n\
         export DOCKER_HOST\n\
         # Find the real CLI, ignoring this directory so the wrapper cannot\n\
         # invoke itself.\n\
         real=\"\"\n\
         IFS=:\n\
         for dir in $PATH; do\n\
         \x20 [ \"$dir\" = \"{WRAPPER_DIR}\" ] && continue\n\
         \x20 if [ -x \"$dir/docker\" ]; then real=\"$dir/docker\"; break; fi\n\
         done\n\
         unset IFS\n\
         if [ -z \"$real\" ]; then\n\
         \x20 echo 'docker: the Docker CLI is not installed in this image' >&2\n\
         \x20 echo '  fix: add a step that installs it, or use a job container that ships it' >&2\n\
         \x20 exit 127\n\
         fi\n\
         exec \"$real\" \"$@\"\n\
         GREENLIT_DOCKER_WRAPPER\n\
         chmod 0755 {WRAPPER_DIR}/docker"
    )
}
