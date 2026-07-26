//! Runtime lifecycle and job-log sink implementations.

use std::io::{self, Write};

use greenlit_engine::Conclusion;
use greenlit_runtime::{ExecutionEvent, ExecutionEventSink, JobScope, RunLogSink, StepEventKind};

use super::{ActiveStep, LineBuffer, RunEvent, RunEventRecorder, preparation};

impl RunLogSink for RunEventRecorder {
    fn write(&mut self, scope: &JobScope, bytes: &[u8]) -> io::Result<usize> {
        let mut completed = Vec::new();
        {
            let mut state = self.lock();
            let buffer = state
                .line_buffers
                .entry(scope.instance_id.clone())
                .or_insert_with(|| LineBuffer {
                    scope: scope.clone(),
                    bytes: Vec::new(),
                });
            buffer.bytes.extend_from_slice(bytes);
            while let Some(position) = buffer.bytes.iter().position(|byte| *byte == b'\n') {
                let mut line = buffer.bytes.drain(..=position).collect::<Vec<_>>();
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                completed.push(String::from_utf8_lossy(&line).into_owned());
            }
        }
        for line in completed {
            self.record_log_line(scope, line, false)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.lock();
        state.journal.flush()?;
        state.output.flush()
    }
}

impl ExecutionEventSink for RunEventRecorder {
    fn on_event(&mut self, event: ExecutionEvent) {
        let result = match event {
            ExecutionEvent::JobStarted { scope } => self.record(RunEvent::JobStarted {
                job_id: scope.job_id,
                instance_id: scope.instance_id,
                display: scope.display,
            }),
            ExecutionEvent::JobSkipped { scope, reason } => self.record(RunEvent::JobSkipped {
                job_id: scope.job_id,
                instance_id: scope.instance_id,
                display: scope.display,
                reason,
            }),
            ExecutionEvent::JobFinished {
                scope,
                conclusion,
                duration,
            } => self.record(RunEvent::JobFinished {
                job_id: scope.job_id,
                instance_id: scope.instance_id,
                display: scope.display,
                conclusion: conclusion_name(conclusion),
                duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
            }),
            ExecutionEvent::StepStarted {
                scope,
                event_id,
                index,
                step_id,
                label,
                kind,
            } => self.step_started(scope, event_id, index, step_id, label, kind),
            ExecutionEvent::StepSkipped {
                scope,
                event_id,
                index,
                step_id,
                label,
                reason,
            } => self.record(RunEvent::StepSkipped {
                job_id: scope.job_id,
                instance_id: scope.instance_id,
                event_id,
                index,
                step_id,
                label,
                reason,
            }),
            ExecutionEvent::StepFinished {
                scope,
                event_id,
                index,
                step_id,
                label,
                outcome,
                conclusion,
                duration,
            } => {
                let result = self.flush_scope_line(&scope).and_then(|()| {
                    self.record(RunEvent::StepFinished {
                        job_id: scope.job_id.clone(),
                        instance_id: scope.instance_id.clone(),
                        event_id,
                        index,
                        step_id,
                        label,
                        outcome: conclusion_name(outcome),
                        conclusion: conclusion_name(conclusion),
                        duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
                    })
                });
                self.lock().active_steps.remove(&scope.instance_id);
                result
            }
            _ => self.record(preparation(
                "execution",
                "unrecognized_event",
                Some("runtime emitted an event this renderer does not understand".to_string()),
            )),
        };
        if let Err(error) = result {
            self.remember_failure(io::Error::other(error));
        }
    }
}

impl RunEventRecorder {
    fn step_started(
        &self,
        scope: JobScope,
        event_id: String,
        index: usize,
        step_id: Option<String>,
        label: String,
        kind: StepEventKind,
    ) -> anyhow::Result<()> {
        let (kind, reference) = match kind {
            StepEventKind::Run => ("run".to_string(), None),
            StepEventKind::Uses { reference } => ("uses".to_string(), Some(reference)),
            StepEventKind::ActionPre { reference } => ("action_pre".to_string(), Some(reference)),
            StepEventKind::ActionPost { reference } => ("action_post".to_string(), Some(reference)),
        };
        self.lock().active_steps.insert(
            scope.instance_id.clone(),
            ActiveStep {
                event_id: event_id.clone(),
            },
        );
        self.record(RunEvent::StepStarted {
            job_id: scope.job_id,
            instance_id: scope.instance_id,
            event_id,
            index,
            step_id,
            label,
            kind,
            reference,
        })
    }
}

fn conclusion_name(conclusion: Conclusion) -> String {
    conclusion.as_github_str().to_string()
}
