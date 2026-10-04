use std::collections::VecDeque;

use harness_review::ReviewReport;
use harness_tools::{FileChange, FileChangeKind, ToolStatus};
use harness_types::{Message, Role};

use super::events::UiEvent;

const MAX_CONVERSATION_LINES: usize = 4_000;
const MAX_TOOL_CARDS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Work,
    Sessions,
    Reviews,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunState {
    Idle,
    Working,
    WaitingForApproval,
}

impl RunState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Working => "Working",
            Self::WaitingForApproval => "Approval needed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolState {
    Running,
    WaitingForApproval,
    Completed,
    Denied,
    Failed,
    TimedOut,
}

impl ToolState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::WaitingForApproval => "Waiting approval",
            Self::Completed => "Completed",
            Self::Denied => "Denied",
            Self::Failed => "Failed",
            Self::TimedOut => "Timed out",
        }
    }

    pub(crate) fn symbol(self) -> &'static str {
        match self {
            Self::Running => "›",
            Self::WaitingForApproval => "?",
            Self::Completed => "✓",
            Self::Denied => "⊘",
            Self::Failed => "×",
            Self::TimedOut => "⌛",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCard {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) args: String,
    pub(crate) result: String,
    pub(crate) state: ToolState,
}

#[derive(Debug, Clone)]
pub(crate) struct ChangedFile {
    pub(crate) path: String,
    pub(crate) kind: FileChangeKind,
}

#[derive(Debug, Clone)]
pub(crate) struct TodoItem {
    pub(crate) text: String,
    pub(crate) state: TodoState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoState {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone)]
pub(crate) struct Approval {
    pub(crate) request_id: String,
    pub(crate) command: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ConversationLine {
    pub(crate) text: String,
    pub(crate) kind: ConversationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationKind {
    User,
    Assistant,
    Notice,
}

pub(crate) struct App {
    pub(crate) workspace: String,
    pub(crate) model: String,
    pub(crate) run_state: RunState,
    pub(crate) view: View,
    pub(crate) sidebar_visible: bool,
    pub(crate) details_visible: bool,
    pub(crate) palette_visible: bool,
    pub(crate) composer: String,
    pub(crate) conversation: VecDeque<ConversationLine>,
    pub(crate) conversation_scroll: usize,
    pub(crate) tools: VecDeque<ToolCard>,
    pub(crate) selected_tool: usize,
    pub(crate) changed_files: Vec<ChangedFile>,
    pub(crate) todos: Vec<TodoItem>,
    pub(crate) approval: Option<Approval>,
    pub(crate) notices: VecDeque<String>,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) session_cost_usd: f64,
    assistant_stream_open: bool,
    pub(crate) sessions: Vec<harness_agent::SessionSummary>,
    pub(crate) reviews: Vec<ReviewReport>,
}

impl App {
    pub(crate) fn new(
        workspace: String,
        model: String,
        history: &[Message],
        sessions: Vec<harness_agent::SessionSummary>,
        reviews: Vec<ReviewReport>,
    ) -> Self {
        let mut app = Self {
            workspace,
            model,
            run_state: RunState::Idle,
            view: View::Work,
            sidebar_visible: true,
            details_visible: false,
            palette_visible: false,
            composer: String::new(),
            conversation: VecDeque::new(),
            conversation_scroll: 0,
            tools: VecDeque::new(),
            selected_tool: 0,
            changed_files: Vec::new(),
            todos: Vec::new(),
            approval: None,
            notices: VecDeque::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            session_cost_usd: 0.0,
            assistant_stream_open: false,
            sessions,
            reviews,
        };
        for message in history {
            match message.role {
                Role::User => app.push_message(ConversationKind::User, &message.content),
                Role::Assistant if !message.content.is_empty() => {
                    app.push_message(ConversationKind::Assistant, &message.content)
                }
                Role::Tool => app.push_message(
                    ConversationKind::Notice,
                    &format!(
                        "{}: {}",
                        message.name.as_deref().unwrap_or("tool"),
                        message.content
                    ),
                ),
                _ => {}
            }
        }
        app
    }

    pub(crate) fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::TurnStarted => self.run_state = RunState::Working,
            UiEvent::AssistantDelta(text) => self.append_assistant_delta(&text),
            UiEvent::AssistantDone => self.assistant_stream_open = false,
            UiEvent::ToolPending { name } => self.push_notice(format!("Preparing {name}")),
            UiEvent::ToolStarted {
                call_id,
                name,
                args,
            } => {
                self.tools.push_back(ToolCard {
                    call_id,
                    name,
                    args,
                    result: String::new(),
                    state: ToolState::Running,
                });
                self.trim_tools();
                self.selected_tool = self.tools.len().saturating_sub(1);
            }
            UiEvent::ToolFinished {
                call_id,
                name,
                result,
                status,
                changed_files,
            } => {
                if let Some(card) = self
                    .tools
                    .iter_mut()
                    .rev()
                    .find(|card| card.call_id == call_id)
                {
                    card.result = result.clone();
                    card.state = tool_state(status);
                } else {
                    self.tools.push_back(ToolCard {
                        call_id,
                        name: name.clone(),
                        args: String::new(),
                        result: result.clone(),
                        state: tool_state(status),
                    });
                    self.trim_tools();
                }
                self.record_changes(changed_files);
                if name == "todo_write" && status == ToolStatus::Ok {
                    self.todos = parse_todos(&result);
                }
            }
            UiEvent::Usage {
                model,
                prompt_tokens,
                completion_tokens,
                session_cost_usd,
            } => {
                self.model = model;
                self.prompt_tokens = prompt_tokens;
                self.completion_tokens = completion_tokens;
                self.session_cost_usd = session_cost_usd;
            }
            UiEvent::ModelChanged(model) => self.model = model,
            UiEvent::RunFinished { error } => self.run_finished(error.map_or(Ok(()), Err)),
            UiEvent::ApprovalRequested {
                request_id,
                command,
            } => {
                self.run_state = RunState::WaitingForApproval;
                if let Some(card) = self
                    .tools
                    .iter_mut()
                    .rev()
                    .find(|card| card.name == "run_shell" && card.state == ToolState::Running)
                {
                    card.state = ToolState::WaitingForApproval;
                }
                self.approval = Some(Approval {
                    request_id,
                    command,
                });
            }
            UiEvent::Notice(message) => self.push_notice(message),
            UiEvent::RunStopped { message } => {
                self.run_state = RunState::Idle;
                self.push_notice(message);
            }
        }
    }

    pub(crate) fn submitted(&mut self, text: &str) {
        self.push_message(ConversationKind::User, text);
        self.composer.clear();
        self.conversation_scroll = 0;
    }

    pub(crate) fn run_finished(&mut self, result: Result<(), String>) {
        if self.approval.is_none() {
            self.run_state = RunState::Idle;
        }
        if let Err(error) = result {
            self.push_notice(error);
        }
    }

    pub(crate) fn resolve_approval(&mut self, approved: bool) -> Option<String> {
        let approval = self.approval.take()?;
        self.run_state = RunState::Working;
        if let Some(card) =
            self.tools.iter_mut().rev().find(|card| {
                card.name == "run_shell" && card.state == ToolState::WaitingForApproval
            })
        {
            card.state = if approved {
                ToolState::Running
            } else {
                ToolState::Denied
            };
        }
        self.push_notice(if approved {
            "Approved command".into()
        } else {
            "Denied command".into()
        });
        Some(approval.request_id)
    }

    pub(crate) fn select_next_tool(&mut self, backwards: bool) {
        if self.tools.is_empty() {
            return;
        }
        if backwards {
            self.selected_tool = self
                .selected_tool
                .checked_sub(1)
                .unwrap_or(self.tools.len() - 1);
        } else {
            self.selected_tool = (self.selected_tool + 1) % self.tools.len();
        }
    }

    pub(crate) fn clear_local_conversation(&mut self) {
        self.conversation.clear();
        self.conversation_scroll = 0;
        self.push_notice("Cleared this screen only; the agent session is unchanged".into());
    }

    fn append_assistant_delta(&mut self, delta: &str) {
        if !self.assistant_stream_open {
            self.conversation.push_back(ConversationLine {
                text: "HiveMind".into(),
                kind: ConversationKind::Assistant,
            });
            self.conversation.push_back(ConversationLine {
                text: String::new(),
                kind: ConversationKind::Assistant,
            });
            self.assistant_stream_open = true;
        }
        for part in delta.split_inclusive('\n') {
            if part.ends_with('\n') {
                if let Some(line) = self.conversation.back_mut() {
                    line.text.push_str(part.trim_end_matches('\n'));
                }
                self.conversation.push_back(ConversationLine {
                    text: String::new(),
                    kind: ConversationKind::Assistant,
                });
            } else if let Some(line) = self.conversation.back_mut() {
                line.text.push_str(part);
            }
        }
        self.trim_conversation();
    }

    fn push_message(&mut self, kind: ConversationKind, text: &str) {
        let title = match kind {
            ConversationKind::User => "You",
            ConversationKind::Assistant => "HiveMind",
            ConversationKind::Notice => "HiveMind",
        };
        self.conversation.push_back(ConversationLine {
            text: title.into(),
            kind,
        });
        for line in text.lines().chain(text.is_empty().then_some("")) {
            self.conversation.push_back(ConversationLine {
                text: line.to_string(),
                kind,
            });
        }
        self.conversation.push_back(ConversationLine {
            text: String::new(),
            kind,
        });
        self.trim_conversation();
    }

    fn push_notice(&mut self, message: String) {
        self.notices.push_back(message.clone());
        while self.notices.len() > 8 {
            self.notices.pop_front();
        }
        self.push_message(ConversationKind::Notice, &message);
    }

    fn record_changes(&mut self, changes: Vec<FileChange>) {
        for change in changes {
            if let Some(existing) = self
                .changed_files
                .iter_mut()
                .find(|file| file.path == change.path)
            {
                existing.kind = change.kind;
            } else {
                self.changed_files.push(ChangedFile {
                    path: change.path,
                    kind: change.kind,
                });
            }
        }
    }

    fn trim_conversation(&mut self) {
        if self.conversation.len() <= MAX_CONVERSATION_LINES {
            return;
        }
        let remove = self.conversation.len() - MAX_CONVERSATION_LINES + 1;
        self.conversation.drain(..remove);
        self.conversation.push_front(ConversationLine {
            text: "Earlier display output was removed to keep the interface responsive.".into(),
            kind: ConversationKind::Notice,
        });
    }

    fn trim_tools(&mut self) {
        while self.tools.len() > MAX_TOOL_CARDS {
            self.tools.pop_front();
        }
    }
}

fn tool_state(status: ToolStatus) -> ToolState {
    match status {
        ToolStatus::Ok => ToolState::Completed,
        ToolStatus::Denied => ToolState::Denied,
        ToolStatus::Failed => ToolState::Failed,
        ToolStatus::Timeout => ToolState::TimedOut,
    }
}

fn parse_todos(result: &str) -> Vec<TodoItem> {
    result
        .lines()
        .filter_map(|line| {
            let (state, text) = if let Some(text) = line.strip_prefix("[x] ") {
                (TodoState::Completed, text)
            } else if let Some(text) = line.strip_prefix("[~] ") {
                (TodoState::InProgress, text)
            } else {
                let text = line.strip_prefix("[ ] ")?;
                (TodoState::Pending, text)
            };
            Some(TodoItem {
                text: text.to_string(),
                state,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_unicode_is_coalesced_into_the_current_assistant_message() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.apply(UiEvent::AssistantDelta("Hello ".into()));
        app.apply(UiEvent::AssistantDelta("世界".into()));
        assert!(app.conversation.back().unwrap().text.contains("世界"));
    }

    #[test]
    fn todo_output_replaces_the_visible_task_list() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.apply(UiEvent::ToolFinished {
            call_id: "a".into(),
            name: "todo_write".into(),
            result: "[x] Read\n[~] Build\n[ ] Test".into(),
            status: ToolStatus::Ok,
            changed_files: Vec::new(),
        });
        assert_eq!(app.todos.len(), 3);
        assert_eq!(app.todos[1].state, TodoState::InProgress);
    }

    #[test]
    fn approval_is_bound_to_its_request_id() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.apply(UiEvent::ApprovalRequested {
            request_id: "request-7".into(),
            command: "cargo test".into(),
        });
        assert_eq!(app.resolve_approval(true).as_deref(), Some("request-7"));
    }

    #[test]
    fn parallel_tool_calls_keep_their_call_ids_and_statuses_separate() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        for (call_id, name) in [("call-1", "read_file"), ("call-2", "edit_file")] {
            app.apply(UiEvent::ToolStarted {
                call_id: call_id.into(),
                name: name.into(),
                args: "{}".into(),
            });
        }
        app.apply(UiEvent::ToolFinished {
            call_id: "call-2".into(),
            name: "edit_file".into(),
            result: "updated".into(),
            status: ToolStatus::Ok,
            changed_files: vec![FileChange {
                path: "src/main.rs".into(),
                kind: FileChangeKind::Modified,
            }],
        });
        app.apply(UiEvent::ToolFinished {
            call_id: "call-1".into(),
            name: "read_file".into(),
            result: "missing".into(),
            status: ToolStatus::Failed,
            changed_files: Vec::new(),
        });
        assert_eq!(app.tools.len(), 2);
        assert_eq!(app.tools[0].call_id, "call-1");
        assert_eq!(app.tools[0].state, ToolState::Failed);
        assert_eq!(app.tools[1].call_id, "call-2");
        assert_eq!(app.tools[1].state, ToolState::Completed);
        assert_eq!(app.changed_files[0].path, "src/main.rs");
    }

    #[test]
    fn denied_and_timed_out_tools_have_distinct_states() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        for (call_id, status) in [
            ("denied", ToolStatus::Denied),
            ("timed", ToolStatus::Timeout),
        ] {
            app.apply(UiEvent::ToolFinished {
                call_id: call_id.into(),
                name: "run_shell".into(),
                result: "result".into(),
                status,
                changed_files: Vec::new(),
            });
        }
        assert_eq!(app.tools[0].state, ToolState::Denied);
        assert_eq!(app.tools[1].state, ToolState::TimedOut);
    }

    #[test]
    fn long_streamed_output_stays_bounded_with_an_explicit_notice() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        for _ in 0..MAX_CONVERSATION_LINES + 20 {
            app.apply(UiEvent::AssistantDelta("line\n".into()));
        }
        assert!(app.conversation.len() <= MAX_CONVERSATION_LINES);
        assert!(
            app.conversation
                .front()
                .unwrap()
                .text
                .contains("Earlier display output")
        );
    }

    #[test]
    fn an_interrupted_run_returns_the_composer_to_an_usable_idle_state() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.run_state = RunState::Working;
        app.apply(UiEvent::RunStopped {
            message: "Interrupted".into(),
        });
        assert_eq!(app.run_state, RunState::Idle);
        app.submitted("start another task");
        assert!(
            app.conversation
                .iter()
                .any(|line| line.text == "start another task")
        );
    }
}
