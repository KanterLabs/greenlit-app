//! Projection of runtime preparation observations into stable records.

use std::io;

use greenlit_runtime::{ProgressEvent, ProgressSink, WorkspaceProgress};

use super::{RunEvent, RunEventRecorder, preparation};

impl ProgressSink for RunEventRecorder {
    fn on_progress(&mut self, event: ProgressEvent) {
        let event = match event {
            ProgressEvent::PullStarted { image } => content("started", Some(image), None, None),
            ProgressEvent::PullProgress {
                current_bytes,
                total_bytes,
            } => content("progress", None, Some(current_bytes), total_bytes),
            ProgressEvent::PullFinished {
                image,
                downloaded_bytes,
            } => RunEvent::Preparation {
                phase: "runner content".to_string(),
                state: "finished".to_string(),
                detail: Some(image),
                current_bytes: Some(downloaded_bytes),
                total_bytes: Some(downloaded_bytes),
                cache_hit: Some(false),
            },
            ProgressEvent::ContentResolved {
                item,
                identity,
                cache_hit,
            } => RunEvent::Preparation {
                phase: item,
                state: "resolved".to_string(),
                detail: Some(identity),
                current_bytes: None,
                total_bytes: None,
                cache_hit: Some(cache_hit),
            },
            ProgressEvent::BuildStarted { tag } => build("started", tag),
            ProgressEvent::BuildLine { line } => build("progress", line),
            ProgressEvent::BuildFinished { tag } => build("finished", tag),
            ProgressEvent::BootStarted => preparation("container", "started", None),
            ProgressEvent::BootFinished => preparation("container", "finished", None),
            ProgressEvent::Workspace(workspace) => workspace_event(workspace),
            ProgressEvent::ServiceStarting { service } => {
                preparation("service", "started", Some(service))
            }
            ProgressEvent::ServiceReady { service } => {
                preparation("service", "ready", Some(service))
            }
            ProgressEvent::ActionRuntimeEnsureStarted => {
                preparation("action runtime", "started", None)
            }
            ProgressEvent::ActionRuntimeEnsureFinished => {
                preparation("action runtime", "finished", None)
            }
            _ => preparation(
                "preparation",
                "unrecognized_event",
                Some("runtime emitted progress this renderer does not understand".to_string()),
            ),
        };
        if let Err(error) = self.record(event) {
            self.remember_failure(io::Error::other(error));
        }
    }
}

fn content(
    state: &str,
    detail: Option<String>,
    current_bytes: Option<u64>,
    total_bytes: Option<u64>,
) -> RunEvent {
    RunEvent::Preparation {
        phase: "runner content".to_string(),
        state: state.to_string(),
        detail,
        current_bytes,
        total_bytes,
        cache_hit: None,
    }
}

fn build(state: &str, detail: String) -> RunEvent {
    RunEvent::Preparation {
        phase: "runner build".to_string(),
        state: state.to_string(),
        detail: Some(detail),
        current_bytes: None,
        total_bytes: None,
        cache_hit: None,
    }
}

fn workspace_event(workspace: WorkspaceProgress) -> RunEvent {
    match workspace {
        WorkspaceProgress::FellBack { reason } => {
            preparation("workspace", "fallback", Some(reason))
        }
        WorkspaceProgress::Copying { files, bytes } => RunEvent::Preparation {
            phase: "workspace".to_string(),
            state: "progress".to_string(),
            detail: Some(format!("{files} files")),
            current_bytes: Some(bytes),
            total_bytes: None,
            cache_hit: None,
        },
        WorkspaceProgress::Ready { strategy } => preparation("workspace", "ready", Some(strategy)),
        _ => preparation(
            "workspace",
            "unrecognized_event",
            Some(
                "runtime emitted workspace progress this renderer does not understand".to_string(),
            ),
        ),
    }
}
