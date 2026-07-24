//! The [`ContainerEngine`] trait surface, driven through `&dyn ContainerEngine`
//! with a fake backend — no real Docker daemon.
//!
//! The engine is the port to a true external (the Docker daemon), so faking it
//! is permitted (`TESTING.md`: mock true externals). This pins two design
//! contracts of the trait itself: it is object-safe (usable behind `dyn`), and
//! [`ContainerEngine::exec`] streams stdout and stderr to the sink separately,
//! chunk by chunk, while returning the process exit code — the shape the
//! execution layer's log folding, masking, and command-file parsing depend on.

use async_trait::async_trait;

use greenlit_runtime::engine::{
    BuildSpec, CommitSpec, ContainerEngine, ContainerSpec, ExecOutput, ExecOutputSink, ExecSpec,
};
use greenlit_runtime::error::RuntimeError;
use greenlit_runtime::progress::ProgressSink;

/// One framed output chunk the fake exec replays.
enum Chunk {
    Out(&'static str),
    Err(&'static str),
}

/// A fake engine that records container lifecycle calls and replays a fixed
/// exec output script.
struct FakeEngine {
    exec_script: Vec<Chunk>,
    exit_code: i64,
}

#[async_trait]
impl ContainerEngine for FakeEngine {
    async fn pull_image(
        &self,
        _image: &str,
        _progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn image_exists(&self, _image: &str) -> Result<bool, RuntimeError> {
        Ok(false)
    }

    async fn build_image(
        &self,
        _spec: &BuildSpec,
        _progress: &mut (dyn ProgressSink + Send),
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn commit_container(&self, spec: &CommitSpec) -> Result<String, RuntimeError> {
        Ok(format!("{}:{}", spec.repo, spec.tag))
    }

    async fn create_container(&self, _spec: &ContainerSpec) -> Result<String, RuntimeError> {
        Ok("container-id".to_string())
    }

    async fn start_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn stop_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn remove_container(&self, _id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn exec(
        &self,
        _container: &str,
        _spec: &ExecSpec,
        sink: &mut (dyn ExecOutputSink + Send),
    ) -> Result<ExecOutput, RuntimeError> {
        for chunk in &self.exec_script {
            match chunk {
                Chunk::Out(text) => sink.on_stdout(text.as_bytes()),
                Chunk::Err(text) => sink.on_stderr(text.as_bytes()),
            }
        }
        Ok(ExecOutput {
            exit_code: self.exit_code,
        })
    }

    async fn export_path(&self, _container: &str, _path: &str) -> Result<Vec<u8>, RuntimeError> {
        Ok(Vec::new())
    }

    async fn create_network(&self, name: &str) -> Result<String, RuntimeError> {
        Ok(format!("net-{name}"))
    }

    async fn remove_network(&self, _name: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
}

/// A sink that keeps stdout and stderr in separate buffers.
#[derive(Default)]
struct SplitSink {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ExecOutputSink for SplitSink {
    fn on_stdout(&mut self, chunk: &[u8]) {
        self.stdout.extend_from_slice(chunk);
    }

    fn on_stderr(&mut self, chunk: &[u8]) {
        self.stderr.extend_from_slice(chunk);
    }
}

#[tokio::test]
async fn exec_streams_stdout_and_stderr_separately_with_exit_code() {
    let engine = FakeEngine {
        exec_script: vec![
            Chunk::Out("hello "),
            Chunk::Err("oops"),
            Chunk::Out("world"),
        ],
        exit_code: 3,
    };
    // Drive through the object-safe port, proving `dyn ContainerEngine` works.
    let engine: &dyn ContainerEngine = &engine;

    let id = engine
        .create_container(&ContainerSpec::default())
        .await
        .unwrap();
    engine.start_container(&id).await.unwrap();

    let mut sink = SplitSink::default();
    let out = engine
        .exec(&id, &ExecSpec::default(), &mut sink)
        .await
        .unwrap();

    assert_eq!(out, ExecOutput { exit_code: 3 });
    assert_eq!(sink.stdout, b"hello world");
    assert_eq!(sink.stderr, b"oops");

    engine.remove_container(&id).await.unwrap();
}
