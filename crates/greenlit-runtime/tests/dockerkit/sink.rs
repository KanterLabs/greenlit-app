//! Output collection for live engine calls.

use greenlit_runtime::engine::ExecOutputSink;

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
}

impl ExecOutputSink for CollectSink {
    fn on_stdout(&mut self, chunk: &[u8]) {
        self.stdout.extend_from_slice(chunk);
    }

    fn on_stderr(&mut self, chunk: &[u8]) {
        self.stderr.extend_from_slice(chunk);
    }
}
