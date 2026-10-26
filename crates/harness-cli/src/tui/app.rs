use std::cell::Cell;
use std::collections::VecDeque;

use harness_review::ReviewReport;
use harness_tools::{FileChange, FileChangeKind, ToolStatus};
use harness_types::{Message, Role};
use ratatui::layout::{Position, Rect};

use super::events::UiEvent;

const MAX_CONVERSATION_LINES: usize = 4_000;
const MAX_TOOL_CARDS: usize = 200;
pub(crate) const SCROLL_ROWS: isize = 3;

/// Geometry from the last drawn frame, so input can clamp and route scrolling.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LastLayout {
    pub(crate) conversation: Rect,
    pub(crate) conversation_page: usize,
    pub(crate) conversation_max_top: usize,
    pub(crate) tools: Rect,
    pub(crate) tasks: Rect,
    pub(crate) tasks_max_offset: usize,
}

/// What the footer can honestly say about spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CostStatus {
    /// Local endpoint: no price to quote.
    Local,
    Known,
    /// At least one billed turn had no known price.
    Partial,
}

/// Listed as you type `/` -- VS Code's terminal swallows Ctrl+P and F1.
pub(crate) const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "list commands"),
    ("/model", "<id>  switch model"),
    ("/reasoning", "<level|off>"),
    ("/budget", "<amount|off>"),
    ("/web", "<on|off>"),
    ("/skill", "off  clear the active skill"),
    ("/compact", "summarize older context"),
    ("/undo", "[n]  undo recent turns"),
    ("/cost", "spend so far"),
    ("/status", "session status"),
    ("/work", "conversation view"),
    ("/sessions", "saved sessions"),
    ("/reviews", "saved reviews"),
    ("/clear", "clear this screen"),
    ("/exit", "quit"),
];

/// Commands matching what's typed so far, while still typing the name.
pub(crate) fn slash_matches(composer: &str) -> Vec<(&'static str, &'static str)> {
    if !composer.starts_with('/') || composer.contains(char::is_whitespace) {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|(name, _)| name.starts_with(composer))
        .collect()
}

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
    /// Computed once at start, so drawing never re-diffs an edit.
    pub(crate) summary: String,
    pub(crate) result: String,
    pub(crate) state: ToolState,
}

/// A short, readable description of a call: what it touched, never raw JSON.
pub(crate) fn tool_summary(name: &str, args: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else {
        return args.split_whitespace().collect::<Vec<_>>().join(" ");
    };
    let text = |key: &str| value.get(key).and_then(|v| v.as_str());
    let first_line =
        |key: &str| text(key).map(|s| s.lines().next().unwrap_or_default().to_string());
    match name {
        "todo_write" => {
            if let Some(todos) = value.get("todos").and_then(|v| v.as_array()) {
                let done = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("completed"))
                    .count();
                return format!("{} tasks · {done} done", todos.len());
            }
        }
        "edit_file" => {
            if let (Some(path), Some(old), Some(new)) =
                (text("path"), text("old_string"), text("new_string"))
            {
                let stats = crate::diff::stats(&crate::diff::diff(old, new));
                return format!("{path}  +{} −{}", stats.added, stats.removed);
            }
        }
        "write_file" => {
            if let (Some(path), Some(content)) = (text("path"), text("content")) {
                return format!("{path}  {} lines", content.lines().count());
            }
        }
        "list_dir" => {
            return first_line("path")
                .filter(|p| !p.is_empty())
                .unwrap_or(".".into());
        }
        _ => {}
    }
    for key in ["command", "path", "pattern", "query", "url", "title"] {
        if let Some(line) = first_line(key) {
            return line;
        }
    }
    value
        .as_object()
        .map(|map| map.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
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
    /// First visible wrapped row; `None` follows the newest output.
    pub(crate) conversation_top: Option<usize>,
    pub(crate) layout: Cell<LastLayout>,
    pub(crate) tools: VecDeque<ToolCard>,
    pub(crate) selected_tool: usize,
    pub(crate) changed_files: Vec<ChangedFile>,
    pub(crate) todos: Vec<TodoItem>,
    pub(crate) tasks_offset: usize,
    pub(crate) approval: Option<Approval>,
    pub(crate) notices: VecDeque<String>,
    /// Session totals, summed across every model request.
    pub(crate) session_prompt_tokens: u64,
    pub(crate) session_completion_tokens: u64,
    pub(crate) session_cost_usd: f64,
    pub(crate) cost_status: CostStatus,
    assistant_stream_open: bool,
    /// One "HiveMind" heading per reply, not one per stream segment.
    reply_heading_shown: bool,
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
            conversation_top: None,
            layout: Cell::new(LastLayout::default()),
            tools: VecDeque::new(),
            selected_tool: 0,
            changed_files: Vec::new(),
            todos: Vec::new(),
            tasks_offset: 0,
            approval: None,
            notices: VecDeque::new(),
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            session_cost_usd: 0.0,
            cost_status: CostStatus::Known,
            assistant_stream_open: false,
            reply_heading_shown: false,
            sessions,
            reviews,
        };
        for message in history {
            match message.role {
                Role::User => {
                    app.push_message(ConversationKind::User, &message.content, true);
                    app.reply_heading_shown = false;
                }
                Role::Assistant if !message.content.is_empty() => {
                    let heading = !app.reply_heading_shown;
                    app.push_message(ConversationKind::Assistant, &message.content, heading);
                    app.reply_heading_shown = true;
                }
                // A resumed session's tool output can be whole files; one line is enough.
                Role::Tool => app.push_notice(format!(
                    "{}: {}",
                    message.name.as_deref().unwrap_or("tool"),
                    message.content.lines().next().unwrap_or_default()
                )),
                _ => {}
            }
        }
        app.notices.clear();
        app
    }

    pub(crate) fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::TurnStarted => self.run_state = RunState::Working,
            UiEvent::AssistantDelta(text) => self.append_assistant_delta(&text),
            UiEvent::AssistantDone => self.assistant_stream_open = false,
            UiEvent::ToolStarted {
                call_id,
                name,
                args,
            } => {
                // Auto-follow only if the newest call was selected, so browsing isn't interrupted.
                let following = self.selected_tool + 1 >= self.tools.len();
                self.tools.push_back(ToolCard {
                    call_id,
                    summary: tool_summary(&name, &args),
                    name,
                    args,
                    result: String::new(),
                    state: ToolState::Running,
                });
                self.trim_tools();
                if following {
                    self.selected_tool = self.tools.len().saturating_sub(1);
                }
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
                        summary: String::new(),
                        result: result.clone(),
                        state: tool_state(status),
                    });
                    self.trim_tools();
                }
                self.record_changes(changed_files);
                if name == "todo_write" && status == ToolStatus::Ok {
                    self.todos = parse_todos(&result);
                    // Keep the step being worked on in view; drawing clamps the rest.
                    self.tasks_offset = self
                        .todos
                        .iter()
                        .position(|todo| todo.state == TodoState::InProgress)
                        .map_or(0, |index| index.saturating_sub(1));
                }
            }
            UiEvent::Usage {
                model,
                prompt_tokens,
                completion_tokens,
                turn_cost_usd,
                billed,
                session_cost_usd,
            } => {
                self.model = model;
                self.session_prompt_tokens += prompt_tokens;
                self.session_completion_tokens += completion_tokens;
                self.session_cost_usd = session_cost_usd;
                self.cost_status = match (billed, turn_cost_usd, self.cost_status) {
                    (false, _, _) => CostStatus::Local,
                    (true, None, _) | (true, Some(_), CostStatus::Partial) => CostStatus::Partial,
                    (true, Some(_), _) => CostStatus::Known,
                };
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
        self.push_message(ConversationKind::User, text, true);
        self.composer.clear();
        self.conversation_top = None;
        self.reply_heading_shown = false;
        self.assistant_stream_open = false;
    }

    /// Positive `rows` scrolls toward newer output. Reaching the bottom resumes following.
    pub(crate) fn scroll_conversation(&mut self, rows: isize) {
        let max = self.layout.get().conversation_max_top;
        let top = self.conversation_top.unwrap_or(max).min(max);
        let next = top.saturating_add_signed(rows).min(max);
        self.conversation_top = (next < max).then_some(next);
    }

    pub(crate) fn page_conversation(&mut self, down: bool) {
        let page = self.layout.get().conversation_page.saturating_sub(1).max(1) as isize;
        self.scroll_conversation(if down { page } else { -page });
    }

    pub(crate) fn scroll_conversation_to_top(&mut self) {
        self.conversation_top = (self.layout.get().conversation_max_top > 0).then_some(0);
    }

    pub(crate) fn scroll_conversation_to_latest(&mut self) {
        self.conversation_top = None;
    }

    /// Moves the tool selection without wrapping; the list scrolls along.
    pub(crate) fn scroll_tools(&mut self, steps: isize) {
        if let Some(last) = self.tools.len().checked_sub(1) {
            self.selected_tool = self.selected_tool.saturating_add_signed(steps).min(last);
        }
    }

    pub(crate) fn scroll_tasks(&mut self, rows: isize) {
        let max = self.layout.get().tasks_max_offset;
        self.tasks_offset = self
            .tasks_offset
            .min(max)
            .saturating_add_signed(rows)
            .min(max);
    }

    /// Mouse wheel: scrolls whichever panel is under the pointer.
    pub(crate) fn scroll_at(&mut self, column: u16, row: u16, down: bool) {
        let layout = self.layout.get();
        let pointer = Position::new(column, row);
        if layout.tools.contains(pointer) {
            self.scroll_tools(if down { 1 } else { -1 });
        } else if layout.tasks.contains(pointer) {
            self.scroll_tasks(if down { 1 } else { -1 });
        } else {
            self.scroll_conversation(if down { SCROLL_ROWS } else { -SCROLL_ROWS });
        }
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
        let verdict = if approved { "Approved" } else { "Denied" };
        let command = approval.command.lines().next().unwrap_or_default();
        self.push_notice(format!("{verdict}: {command}"));
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
        self.conversation_top = None;
        self.push_notice("Cleared this screen only; the agent session is unchanged".into());
    }

    fn append_assistant_delta(&mut self, delta: &str) {
        if !self.assistant_stream_open {
            if !self.reply_heading_shown {
                self.conversation.push_back(ConversationLine {
                    text: "HiveMind".into(),
                    kind: ConversationKind::Assistant,
                });
                self.reply_heading_shown = true;
            } else if self.conversation.back().is_some_and(|l| !l.text.is_empty()) {
                // Later segments of the same reply: a blank line, not a new heading.
                self.conversation.push_back(ConversationLine {
                    text: String::new(),
                    kind: ConversationKind::Assistant,
                });
            }
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

    fn push_message(&mut self, kind: ConversationKind, text: &str, heading: bool) {
        if heading {
            let title = match kind {
                ConversationKind::User => "You",
                ConversationKind::Assistant | ConversationKind::Notice => "HiveMind",
            };
            self.conversation.push_back(ConversationLine {
                text: title.into(),
                kind,
            });
        }
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

    /// One dim line per notice, so status chatter doesn't crowd out replies.
    fn push_notice(&mut self, message: String) {
        self.notices.push_back(message.clone());
        while self.notices.len() > 8 {
            self.notices.pop_front();
        }
        self.assistant_stream_open = false;
        for line in message.lines() {
            self.conversation.push_back(ConversationLine {
                text: format!("· {line}"),
                kind: ConversationKind::Notice,
            });
        }
        self.trim_conversation();
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
            // Keep pointing at the same card after the front shifts away.
            self.selected_tool = self.selected_tool.saturating_sub(1);
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

    fn usage_event(prompt: u64, cost: Option<f64>, billed: bool, session: f64) -> UiEvent {
        UiEvent::Usage {
            model: "m".into(),
            prompt_tokens: prompt,
            completion_tokens: 10,
            turn_cost_usd: cost,
            billed,
            session_cost_usd: session,
        }
    }

    #[test]
    fn usage_accumulates_session_totals_and_never_fakes_a_known_cost() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.apply(usage_event(1_000, Some(0.01), true, 0.01));
        app.apply(usage_event(2_000, Some(0.02), true, 0.03));
        assert_eq!(app.session_prompt_tokens, 3_000);
        assert_eq!(app.session_completion_tokens, 20);
        assert_eq!(app.cost_status, CostStatus::Known);

        app.apply(usage_event(500, None, true, 0.03));
        assert_eq!(app.cost_status, CostStatus::Partial);
        app.apply(usage_event(500, Some(0.01), true, 0.04));
        assert_eq!(
            app.cost_status,
            CostStatus::Partial,
            "an unknown turn stays counted"
        );

        let mut local = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        local.apply(usage_event(100, None, false, 0.0));
        assert_eq!(local.cost_status, CostStatus::Local);
    }

    #[test]
    fn a_reply_split_by_tool_calls_gets_one_heading() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.submitted("build it");
        app.apply(UiEvent::AssistantDelta("Reading files.".into()));
        app.apply(UiEvent::AssistantDone);
        app.apply(UiEvent::Notice("Approved: ls".into()));
        app.apply(UiEvent::AssistantDelta("Done.".into()));
        let headings = |app: &App| {
            app.conversation
                .iter()
                .filter(|l| l.text == "HiveMind")
                .count()
        };
        assert_eq!(headings(&app), 1);

        app.submitted("next");
        app.apply(UiEvent::AssistantDelta("Again.".into()));
        assert_eq!(headings(&app), 2, "a new request starts a new reply");
    }

    #[test]
    fn approval_notices_name_the_command() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.apply(UiEvent::ApprovalRequested {
            request_id: "r".into(),
            command: "curl -s http://localhost:8080\nsecond line".into(),
        });
        app.resolve_approval(true);
        assert!(
            app.conversation
                .iter()
                .any(|l| l.text == "· Approved: curl -s http://localhost:8080")
        );
    }

    #[test]
    fn tool_summaries_are_readable_not_raw_json() {
        let todos = r#"{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"pending"}]}"#;
        assert_eq!(tool_summary("todo_write", todos), "2 tasks · 1 done");
        let edit = r#"{"path":"script.js","old_string":"a\nb","new_string":"a\nc\nd"}"#;
        assert_eq!(tool_summary("edit_file", edit), "script.js  +2 −1");
        let write = r#"{"path":"index.html","content":"<p>\n</p>\n"}"#;
        assert_eq!(tool_summary("write_file", write), "index.html  2 lines");
        assert_eq!(tool_summary("list_dir", "{}"), ".");
        assert_eq!(
            tool_summary("run_shell", r#"{"command":"ls -la\npwd"}"#),
            "ls -la"
        );
        assert_eq!(
            tool_summary("mystery", r#"{"alpha":1,"beta":[2]}"#),
            "alpha, beta"
        );
    }

    #[test]
    fn a_todo_update_keeps_the_current_step_in_view() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        let list = (0..12)
            .map(|i| {
                format!(
                    "[{}] step {i}",
                    if i < 8 {
                        "x"
                    } else if i == 8 {
                        "~"
                    } else {
                        " "
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        app.apply(UiEvent::ToolFinished {
            call_id: "t".into(),
            name: "todo_write".into(),
            result: list,
            status: ToolStatus::Ok,
            changed_files: Vec::new(),
        });
        assert_eq!(app.tasks_offset, 7);
        app.layout.set(LastLayout {
            tasks_max_offset: 9,
            ..LastLayout::default()
        });
        app.scroll_tasks(100);
        assert_eq!(app.tasks_offset, 9);
        app.scroll_tasks(-100);
        assert_eq!(app.tasks_offset, 0);
    }

    #[test]
    fn a_resumed_sessions_tool_output_is_one_line() {
        let history = vec![
            Message::user("show it"),
            Message::tool_result("c1", "read_file", "line one\nline two\nline three"),
        ];
        let app = App::new(
            "work".into(),
            "model".into(),
            &history,
            Vec::new(),
            Vec::new(),
        );
        assert!(
            app.conversation
                .iter()
                .any(|l| l.text == "· read_file: line one")
        );
        assert!(!app.conversation.iter().any(|l| l.text.contains("line two")));
    }

    #[test]
    fn tool_scrolling_stops_at_the_ends_and_the_newest_resumes_following() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        let start = |app: &mut App, id: &str| {
            app.apply(UiEvent::ToolStarted {
                call_id: id.into(),
                name: "read_file".into(),
                args: "{}".into(),
            })
        };
        for id in ["a", "b", "c"] {
            start(&mut app, id);
        }
        app.scroll_tools(-10);
        assert_eq!(app.selected_tool, 0);
        start(&mut app, "d");
        assert_eq!(
            app.selected_tool, 0,
            "browsing an old call is not interrupted"
        );
        app.scroll_tools(10);
        assert_eq!(app.selected_tool, 3);
        start(&mut app, "e");
        assert_eq!(
            app.selected_tool, 4,
            "at the newest call, the list follows again"
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
