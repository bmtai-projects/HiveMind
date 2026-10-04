use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness_agent::{ToolEvent, Ui};
use harness_config::Backend;
use harness_tools::{FileChange, ToolStatus};
use harness_types::Usage;

const MAX_QUEUED_EVENTS: usize = 512;

/// Events produced by the agent and consumed by the TUI event loop.  Keeping
/// this boundary typed means the screen never has to infer a tool outcome by
/// inspecting its printed output.
#[derive(Debug, Clone)]
pub(crate) enum UiEvent {
    TurnStarted,
    AssistantDelta(String),
    AssistantDone,
    ToolPending {
        name: String,
    },
    ToolStarted {
        call_id: String,
        name: String,
        args: String,
    },
    ToolFinished {
        call_id: String,
        name: String,
        result: String,
        status: ToolStatus,
        changed_files: Vec<FileChange>,
    },
    Usage {
        model: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        session_cost_usd: f64,
    },
    ModelChanged(String),
    RunFinished {
        error: Option<String>,
    },
    ApprovalRequested {
        request_id: String,
        command: String,
    },
    Notice(String),
    RunStopped {
        message: String,
    },
}

impl UiEvent {
    fn is_droppable(&self) -> bool {
        matches!(
            self,
            Self::AssistantDelta(_) | Self::ToolPending { .. } | Self::Notice(_)
        )
    }

    fn is_stream_delta(&self) -> bool {
        matches!(self, Self::AssistantDelta(_))
    }
}

/// A bounded queue with one deliberate exception: lifecycle events are never
/// discarded. Under heavy streaming, adjacent assistant deltas are combined;
/// progress-only notices may be skipped when the queue is full.
#[derive(Default)]
pub(crate) struct EventQueue {
    events: Mutex<VecDeque<UiEvent>>,
}

impl EventQueue {
    pub(crate) fn push(&self, event: UiEvent) {
        let mut events = self.events.lock().expect("TUI event queue poisoned");
        if event.is_stream_delta()
            && let Some(UiEvent::AssistantDelta(previous)) = events.back_mut()
            && let UiEvent::AssistantDelta(next) = event
        {
            previous.push_str(&next);
            return;
        }

        if events.len() >= MAX_QUEUED_EVENTS {
            if event.is_droppable() {
                return;
            }
            if let Some(index) = events.iter().position(UiEvent::is_droppable) {
                events.remove(index);
            }
        }
        events.push_back(event);
    }

    pub(crate) fn drain(&self) -> Vec<UiEvent> {
        self.events
            .lock()
            .expect("TUI event queue poisoned")
            .drain(..)
            .collect()
    }
}

/// Adapter between synchronous agent callbacks and the TUI's single owner
/// event loop. Shell approval is intentionally a request/reply handshake so
/// concurrent calls cannot approve one another by accident.
pub(crate) struct TuiBridge {
    queue: Arc<EventQueue>,
    approvals: Mutex<std::collections::HashMap<String, std::sync::mpsc::Sender<bool>>>,
    next_request_id: AtomicU64,
}

impl TuiBridge {
    pub(crate) fn new() -> Self {
        Self {
            queue: Arc::new(EventQueue::default()),
            approvals: Mutex::new(std::collections::HashMap::new()),
            next_request_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn drain(&self) -> Vec<UiEvent> {
        self.queue.drain()
    }

    pub(crate) fn request_shell_approval(&self, command: &str) -> bool {
        let request_id = format!(
            "tui-appr-{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        self.approvals
            .lock()
            .expect("TUI approvals poisoned")
            .insert(request_id.clone(), sender);
        self.queue.push(UiEvent::ApprovalRequested {
            request_id: request_id.clone(),
            command: command.to_string(),
        });
        // Bash runs this callback on its blocking worker. A failed or late
        // response denies the command, matching the existing terminal flow.
        let approved = receiver.recv().unwrap_or(false);
        self.approvals
            .lock()
            .expect("TUI approvals poisoned")
            .remove(&request_id);
        approved
    }

    pub(crate) fn resolve_approval(&self, request_id: &str, approved: bool) {
        if let Some(sender) = self
            .approvals
            .lock()
            .expect("TUI approvals poisoned")
            .remove(request_id)
        {
            let _ = sender.send(approved);
        }
    }

    pub(crate) fn stopped(&self, message: impl Into<String>) {
        self.queue.push(UiEvent::RunStopped {
            message: message.into(),
        });
    }

    pub(crate) fn notice(&self, message: impl Into<String>) {
        self.queue.push(UiEvent::Notice(message.into()));
    }

    pub(crate) fn model_changed(&self, model: impl Into<String>) {
        self.queue.push(UiEvent::ModelChanged(model.into()));
    }

    pub(crate) fn run_finished(&self, result: anyhow::Result<()>) {
        self.queue.push(UiEvent::RunFinished {
            error: result.err().map(|error| format!("{error:#}")),
        });
    }
}

impl Ui for TuiBridge {
    fn diagnostic(&self, message: &str) {
        self.queue.push(UiEvent::Notice(message.to_string()));
    }

    fn turn_started(&self) {
        self.queue.push(UiEvent::TurnStarted);
    }

    fn assistant_delta(&self, text: &str) {
        self.queue.push(UiEvent::AssistantDelta(text.to_string()));
    }

    fn reasoning_delta(&self, _text: &str) {
        // The ordinary TUI intentionally keeps raw reasoning out of the
        // conversation, just as the plain terminal does by default.
    }

    fn assistant_done(&self) {
        self.queue.push(UiEvent::AssistantDone);
    }

    fn tool_call_pending(&self, name: &str) {
        self.queue.push(UiEvent::ToolPending {
            name: name.to_string(),
        });
    }

    fn tool_start(&self, name: &str, args: &str) {
        self.queue.push(UiEvent::ToolPending {
            name: format!("{name} {args}"),
        });
    }

    fn tool_start_detailed(&self, call_id: &str, name: &str, args: &str) {
        self.queue.push(UiEvent::ToolStarted {
            call_id: call_id.to_string(),
            name: name.to_string(),
            args: args.to_string(),
        });
    }

    fn tool_end(
        &self,
        name: &str,
        result: &str,
        is_error: bool,
        _cost_usd: f64,
        _session_cost_usd: f64,
    ) {
        self.queue.push(UiEvent::ToolFinished {
            call_id: format!("legacy-{name}"),
            name: name.to_string(),
            result: result.to_string(),
            status: if is_error {
                ToolStatus::Failed
            } else {
                ToolStatus::Ok
            },
            changed_files: Vec::new(),
        });
    }

    fn tool_end_detailed(&self, event: ToolEvent<'_>) {
        self.queue.push(UiEvent::ToolFinished {
            call_id: event.call_id.to_string(),
            name: event.name.to_string(),
            result: event.result.to_string(),
            status: event.status,
            changed_files: event.changed_files.to_vec(),
        });
    }

    fn usage(&self, usage: &Usage, model_id: &str, _backend: Backend, session_cost_usd: f64) {
        self.queue.push(UiEvent::Usage {
            model: model_id.to_string(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            session_cost_usd,
        });
    }

    fn retrying(&self, attempt: u32, max: u32, delay: Duration, err: &str) {
        self.queue.push(UiEvent::Notice(format!(
            "Retry {attempt}/{max} in {:.1}s: {err}",
            delay.as_secs_f64()
        )));
    }

    fn model_escalated(&self, from: &str, to: &str, reason: &str) {
        self.queue.push(UiEvent::Notice(format!(
            "Model changed from {from} to {to}: {reason}"
        )));
    }

    fn interjected(&self, count: usize) {
        self.queue.push(UiEvent::Notice(format!(
            "Queued {count} steering message(s)"
        )));
    }

    fn compacted(
        &self,
        messages_before: usize,
        messages_after: usize,
        _tokens_before: u64,
        _summary_cost_usd: Option<f64>,
    ) {
        self.queue.push(UiEvent::Notice(format!(
            "Compacted context from {messages_before} to {messages_after} messages"
        )));
    }

    fn stopped_for_budget(&self, spent_usd: f64, budget_usd: f64) {
        self.queue.push(UiEvent::RunStopped {
            message: format!("Stopped at ${spent_usd:.6} of the ${budget_usd:.2} budget"),
        });
    }

    fn stopped_for_context_limit(&self, estimated_tokens: u64, context_window: u64) {
        self.queue.push(UiEvent::RunStopped {
            message: format!(
                "Stopped at the context limit ({estimated_tokens} of {context_window} tokens)"
            ),
        });
    }

    fn tool_progress(&self, tool: &str, message: &str) {
        self.queue
            .push(UiEvent::Notice(format!("{tool}: {message}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_stream_chunks_coalesce_without_reordering_lifecycle_events() {
        let queue = EventQueue::default();
        queue.push(UiEvent::AssistantDelta("one".into()));
        queue.push(UiEvent::AssistantDelta(" two".into()));
        queue.push(UiEvent::ApprovalRequested {
            request_id: "approval-1".into(),
            command: "cargo test".into(),
        });
        queue.push(UiEvent::AssistantDone);
        let events = queue.drain();
        assert!(matches!(&events[0], UiEvent::AssistantDelta(text) if text == "one two"));
        assert!(
            matches!(&events[1], UiEvent::ApprovalRequested { request_id, .. } if request_id == "approval-1")
        );
        assert!(matches!(&events[2], UiEvent::AssistantDone));
    }

    #[test]
    fn a_full_queue_keeps_an_approval_request() {
        let queue = EventQueue::default();
        for index in 0..MAX_QUEUED_EVENTS {
            queue.push(UiEvent::Notice(format!("progress {index}")));
        }
        queue.push(UiEvent::ApprovalRequested {
            request_id: "approval-2".into(),
            command: "cargo test".into(),
        });
        assert!(queue.drain().iter().any(
            |event| matches!(event, UiEvent::ApprovalRequested { request_id, .. } if request_id == "approval-2")
        ));
    }

    #[test]
    fn approval_response_is_delivered_to_the_exact_request() {
        let bridge = Arc::new(TuiBridge::new());
        let waiting = bridge.clone();
        let join = std::thread::spawn(move || waiting.request_shell_approval("cargo test"));
        let request_id = loop {
            if let Some(UiEvent::ApprovalRequested { request_id, .. }) = bridge.drain().pop() {
                break request_id;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        bridge.resolve_approval(&request_id, true);
        assert!(join.join().unwrap());
    }
}
