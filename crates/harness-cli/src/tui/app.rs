use std::cell::Cell;
use std::collections::VecDeque;

use harness_review::ReviewReport;
use harness_tools::{FileChange, FileChangeKind, ToolStatus};
use harness_types::{Message, Role};
use ratatui::layout::{Position, Rect};

use super::events::{ModelChoice, UiEvent};
use super::listview;

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
    pub(crate) tools_rows: usize,
    pub(crate) tasks: Rect,
    pub(crate) tasks_max_offset: usize,
    pub(crate) files: Rect,
    pub(crate) files_rows: usize,
    pub(crate) sidebar_shown: bool,
    pub(crate) detail_page: usize,
    pub(crate) detail_max_scroll: usize,
    pub(crate) picker: Rect,
    pub(crate) picker_list: Rect,
}

/// The panel that arrow keys, PgUp/PgDn and Enter act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Panel {
    Conversation,
    Tasks,
    Tools,
    Files,
}

/// Where a changed file's before and current contents come from.
pub(crate) trait FileSource {
    /// `None`: no snapshot. `Some(None)`: the file didn't exist before.
    fn before(&self, path: &str) -> Option<Option<String>>;
    fn now(&self, path: &str) -> Option<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DetailKind {
    Tool(String),
    File(usize),
    Changes,
    /// The whole conversation with every tool call expanded.
    Transcript,
    Help,
}

/// A scrollable view over the body. Diffs are frozen when opened; a tool's
/// view is rebuilt every frame so a running call fills in.
#[derive(Debug, Clone)]
pub(crate) struct Detail {
    pub(crate) kind: DetailKind,
    pub(crate) title: String,
    pub(crate) lines: Vec<ratatui::text::Line<'static>>,
    pub(crate) scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerKind {
    Model,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PickerItem {
    /// What gets applied: a model id, or a reasoning level (`off` for none).
    pub(crate) value: String,
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) current: bool,
    /// Why it is listed, in the top picks ("Smartest", "Best value").
    pub(crate) tags: Vec<&'static str>,
}

/// The ways `/model` can order and narrow its list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelView {
    Top,
    Smartest,
    Cheapest,
    Context,
    Popular,
    All,
}

impl ModelView {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Top => "Top 5",
            Self::Smartest => "Smartest",
            Self::Cheapest => "Cheapest",
            Self::Context => "Context",
            Self::Popular => "Popular",
            Self::All => "All",
        }
    }

    /// One plain line saying what the view is, and where its numbers come from.
    pub(crate) fn blurb(self) -> &'static str {
        match self {
            Self::Top => {
                "Five good starting points; the tag says why. $ to $$$$ shows cost, with prices per 1M tokens (input/output)."
            }
            Self::Smartest => {
                "Highest score on the Artificial Analysis intelligence index (0-100), as published by the provider."
            }
            Self::Cheapest => {
                "Cheapest among the strongest quarter by score. Cost counts 3 parts input to 1 part output."
            }
            Self::Context => "Largest context window first, among the strongest quarter by score.",
            Self::Popular => "What people use most for programming, according to the provider.",
            Self::All => "Every model this provider lists. Type to search.",
        }
    }
}

/// A list to choose from, opened by `/model` and `/reasoning`.
#[derive(Debug)]
pub(crate) struct Picker {
    pub(crate) kind: PickerKind,
    pub(crate) title: String,
    /// Shown above the list: a caveat, or why the list is short.
    pub(crate) note: Option<String>,
    pub(crate) items: Vec<PickerItem>,
    pub(crate) filter: String,
    /// Position within [`Picker::visible`].
    pub(crate) selected: usize,
    /// Corrected while drawing so the selection stays in view.
    pub(crate) offset: Cell<usize>,
    pub(crate) loading: bool,
    /// The views on offer, in tab order. One entry means no tabs.
    pub(crate) views: Vec<ModelView>,
    pub(crate) view: ModelView,
    /// Item indices for each ranked view; [`ModelView::All`] has no list.
    pub(crate) lists: Vec<(ModelView, Vec<usize>)>,
}

/// What choosing a row asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickerChoice {
    Model(String),
    /// `None` turns reasoning off.
    Reasoning(Option<String>),
}

impl Picker {
    fn loading(kind: PickerKind, title: &str) -> Self {
        Self {
            kind,
            title: title.into(),
            note: None,
            items: Vec::new(),
            filter: String::new(),
            selected: 0,
            offset: Cell::new(0),
            loading: true,
            views: vec![ModelView::All],
            view: ModelView::All,
            lists: Vec::new(),
        }
    }

    /// Indices into `items` to show: the current view's list, or everything
    /// that matches the filter. The model in use always comes first.
    pub(crate) fn visible(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        let mut shown: Vec<usize> = match self.lists.iter().find(|(view, _)| *view == self.view) {
            Some((_, list)) => list.clone(),
            None => (0..self.items.len()).collect(),
        };
        shown.retain(|&index| {
            let item = &self.items[index];
            needle.is_empty()
                || format!("{} {} {}", item.value, item.label, item.detail)
                    .to_lowercase()
                    .contains(&needle)
        });
        if self.kind == PickerKind::Model {
            let current = self.items.iter().position(|item| item.current);
            if let Some(current) = current {
                match shown.iter().position(|&index| index == current) {
                    Some(at) => {
                        shown.remove(at);
                        shown.insert(0, current);
                    }
                    None if needle.is_empty() => shown.insert(0, current),
                    None => {}
                }
            }
        }
        shown
    }

    /// Switches to the next or previous view, wrapping around.
    pub(crate) fn next_view(&mut self, forward: bool) {
        if self.kind != PickerKind::Model || self.loading || self.views.len() < 2 {
            return;
        }
        let at = self.views.iter().position(|v| *v == self.view).unwrap_or(0);
        let count = self.views.len();
        self.view = self.views[(if forward { at + 1 } else { at + count - 1 }) % count];
        self.selected = 0;
        self.offset.set(0);
    }

    pub(crate) fn chosen(&self) -> Option<&PickerItem> {
        self.visible()
            .get(self.selected)
            .map(|&index| &self.items[index])
    }

    pub(crate) fn move_by(&mut self, delta: isize) {
        let count = self.visible().len();
        if count > 0 {
            self.selected = self.selected.saturating_add_signed(delta).min(count - 1);
        }
    }

    /// Typing searches everything, so it leaves any ranked view.
    pub(crate) fn type_char(&mut self, c: char) {
        if self.kind == PickerKind::Model && !self.loading {
            self.view = ModelView::All;
            self.filter.push(c);
            self.selected = 0;
        }
    }

    pub(crate) fn backspace(&mut self) {
        if self.kind == PickerKind::Model && self.filter.pop().is_some() {
            self.selected = 0;
        }
    }
}

/// "1M", "400k": a context size at a glance.
fn compact_tokens(n: u64) -> String {
    match n {
        1_000_000.. => format!("{}M", n / 1_000_000),
        1_000.. => format!("{}k", n / 1_000),
        _ => n.to_string(),
    }
}

fn price_label(per_million: f64) -> String {
    if per_million >= 0.1 {
        format!("${per_million:.2}")
    } else {
        format!("${per_million:.3}")
    }
}

fn model_detail(choice: &ModelChoice) -> String {
    let mut parts = Vec::new();
    if let (Some(input), Some(output)) = (choice.input_per_m, choice.output_per_m) {
        let tier = super::picks::blended_price(choice).map_or("", super::picks::cost_tier);
        parts.push(format!(
            "{tier} {}/{}",
            price_label(input),
            price_label(output)
        ));
    }
    if let Some(window) = choice.context_window {
        parts.push(format!("{} ctx", compact_tokens(window)));
    }
    if let Some(score) = choice.intelligence {
        parts.push(format!("score {score:.0}"));
    }
    if choice.thinks {
        parts.push("thinks".to_string());
    }
    if let Some(name) = choice.name.as_deref().filter(|n| *n != choice.id) {
        parts.push(name.to_string());
    }
    parts.join("  ·  ")
}

/// A short plain-words reading of a reasoning level.
fn level_blurb(level: &str) -> &'static str {
    match level {
        "off" => "don't ask the model to adjust its thinking",
        "none" => "ask for no extra thinking",
        "minimal" => "barely any extra thinking",
        "low" => "quicker, lighter thinking",
        "medium" => "balanced",
        "high" => "slower, more thorough",
        "xhigh" => "very thorough",
        "max" => "the most thorough, and the slowest",
        _ => "",
    }
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
    ("/help", "keys and commands"),
    ("/model", "choose a model"),
    ("/reasoning", "choose how much it thinks"),
    ("/budget", "<amount|off>"),
    ("/web", "<on|off>"),
    ("/skill", "off  clear the active skill"),
    ("/compact", "summarize older context"),
    ("/undo", "[n]  undo recent turns"),
    ("/diff", "every change this session"),
    ("/transcript", "everything, tool calls expanded"),
    ("/copy", "copy the last reply"),
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

/// What a typed command name means: itself, a unique abbreviation of one,
/// or a prefix several commands share.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    Command(&'static str),
    Ambiguous(Vec<&'static str>),
    Unknown,
}

pub(crate) fn resolve_command(typed: &str) -> Resolved {
    if let Some((name, _)) = SLASH_COMMANDS.iter().find(|(name, _)| *name == typed) {
        return Resolved::Command(name);
    }
    if typed.len() < 2 || !typed.starts_with('/') {
        return Resolved::Unknown;
    }
    let matches: Vec<&'static str> = SLASH_COMMANDS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| name.starts_with(typed))
        .collect();
    match matches.as_slice() {
        [] => Resolved::Unknown,
        [only] => Resolved::Command(only),
        _ => Resolved::Ambiguous(matches),
    }
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

/// A failed or timed-out call's first non-empty result line.
pub(crate) fn failure_reason(card: &ToolCard) -> Option<&str> {
    matches!(card.state, ToolState::Failed | ToolState::TimedOut)
        .then(|| card.result.lines().map(str::trim).find(|l| !l.is_empty()))
        .flatten()
}

/// Rows a tool card takes in the side panel.
pub(crate) fn tool_card_height(card: &ToolCard) -> usize {
    2 + usize::from(failure_reason(card).is_some())
}

#[derive(Debug, Clone)]
pub(crate) struct ChangedFile {
    pub(crate) path: String,
    pub(crate) kind: FileChangeKind,
    /// Lines added and removed since the session started, when known.
    pub(crate) stats: Option<(usize, usize)>,
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
    /// "You" or "HiveMind" above a message.
    Heading,
    User,
    Assistant,
    Notice,
    /// One tool call in the narrative; `text` holds its call id.
    Tool,
}

/// How a tool reads in the conversation: "Edit script.js", not "edit_file".
pub(crate) fn tool_verb(name: &str) -> &str {
    match name {
        "read_file" | "read_program" => "Read",
        "write_file" => "Write",
        "edit_file" => "Edit",
        "run_shell" => "Shell",
        "list_dir" => "List",
        "search" | "semantic_search" => "Search",
        "project_map" => "Map",
        "todo_write" => "Plan",
        "web_search" => "Web search",
        "web_fetch" => "Fetch",
        "create_pdf" => "PDF",
        "create_spreadsheet" => "Spreadsheet",
        "create_diagram" => "Diagram",
        "read_artifact" => "Artifact",
        other => other,
    }
}

pub(crate) struct App {
    pub(crate) workspace: String,
    pub(crate) model: String,
    pub(crate) run_state: RunState,
    pub(crate) view: View,
    pub(crate) sidebar_visible: bool,
    pub(crate) palette_visible: bool,
    pub(crate) focus: Panel,
    pub(crate) detail: Option<Detail>,
    pub(crate) picker: Option<Picker>,
    /// The reasoning level in use, when one is set.
    pub(crate) reasoning: Option<String>,
    pub(crate) composer: String,
    pub(crate) conversation: VecDeque<ConversationLine>,
    /// First visible wrapped row; `None` follows the newest output.
    pub(crate) conversation_top: Option<usize>,
    pub(crate) layout: Cell<LastLayout>,
    pub(crate) tools: VecDeque<ToolCard>,
    pub(crate) selected_tool: usize,
    /// Corrected while drawing so the selection always stays in view.
    pub(crate) tools_offset: Cell<usize>,
    pub(crate) changed_files: Vec<ChangedFile>,
    pub(crate) selected_file: usize,
    pub(crate) files_offset: Cell<usize>,
    files_dirty: bool,
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
    /// When the current request started, for the live status line.
    pub(crate) run_started: Option<std::time::Instant>,
    /// The latest reply's raw text, for `/copy`.
    pub(crate) last_reply: String,
    /// The latest request's prompt size, for context usage.
    pub(crate) last_prompt_tokens: u64,
    /// Where requests go, e.g. "openrouter.ai" or "local · 127.0.0.1:11434".
    pub(crate) connection: String,
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
            palette_visible: false,
            focus: Panel::Conversation,
            detail: None,
            picker: None,
            reasoning: None,
            composer: String::new(),
            conversation: VecDeque::new(),
            conversation_top: None,
            layout: Cell::new(LastLayout::default()),
            tools: VecDeque::new(),
            selected_tool: 0,
            tools_offset: Cell::new(0),
            changed_files: Vec::new(),
            selected_file: 0,
            files_offset: Cell::new(0),
            files_dirty: false,
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
            run_started: None,
            last_reply: String::new(),
            last_prompt_tokens: 0,
            connection: String::new(),
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
            UiEvent::TurnStarted => {
                self.run_state = RunState::Working;
                self.run_started.get_or_insert_with(std::time::Instant::now);
            }
            UiEvent::AssistantDelta(text) => self.append_assistant_delta(&text),
            UiEvent::AssistantDone => self.assistant_stream_open = false,
            UiEvent::ToolStarted {
                call_id,
                name,
                args,
            } => {
                // Auto-follow only if the newest call was selected, so browsing isn't interrupted.
                let following = self.selected_tool + 1 >= self.tools.len();
                self.push_tool_row(&call_id);
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
                    self.push_tool_row(&call_id);
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
                self.last_prompt_tokens = prompt_tokens;
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
            UiEvent::ModelChoices { current, result } => self.fill_model_picker(&current, result),
            UiEvent::ReasoningChoices {
                model,
                current,
                levels,
                verified,
            } => self.fill_reasoning_picker(&model, current, levels, verified),
            UiEvent::ReasoningChanged(level) => self.reasoning = level,
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
                self.run_started = None;
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
        self.last_reply.clear();
    }

    fn push_tool_row(&mut self, call_id: &str) {
        self.assistant_stream_open = false;
        self.conversation.push_back(ConversationLine {
            text: call_id.to_string(),
            kind: ConversationKind::Tool,
        });
        self.trim_conversation();
    }

    /// What the agent is doing right now, for the status line; `None` when idle.
    pub(crate) fn activity(&self) -> Option<String> {
        if self.run_state == RunState::Idle {
            return None;
        }
        if self.run_state == RunState::WaitingForApproval {
            return Some("Waiting for your approval".into());
        }
        if let Some(card) = self
            .tools
            .iter()
            .rev()
            .find(|card| card.state == ToolState::Running)
        {
            return Some(format!("{} {}", tool_verb(&card.name), card.summary));
        }
        Some(
            if self.assistant_stream_open {
                "Writing"
            } else {
                "Thinking"
            }
            .into(),
        )
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

    pub(crate) fn open_model_picker(&mut self) {
        self.picker = Some(Picker::loading(PickerKind::Model, " Choose a model "));
    }

    pub(crate) fn open_reasoning_picker(&mut self) {
        self.picker = Some(Picker::loading(
            PickerKind::Reasoning,
            " How much should it think? ",
        ));
    }

    pub(crate) fn close_picker(&mut self) {
        self.picker = None;
    }

    fn fill_model_picker(&mut self, current: &str, result: Result<Vec<ModelChoice>, String>) {
        let Some(picker) = self
            .picker
            .as_mut()
            .filter(|p| p.kind == PickerKind::Model && p.loading)
        else {
            return;
        };
        let (choices, note) = match result {
            Ok(choices) if choices.is_empty() => (
                Vec::new(),
                Some(
                    "This provider returned no models. Use /model <id> to switch by name."
                        .to_string(),
                ),
            ),
            Ok(choices) => (choices, None),
            Err(why) => (
                Vec::new(),
                Some(format!("{why}. Use /model <id> to switch by name.")),
            ),
        };
        let mut items: Vec<PickerItem> = choices
            .iter()
            .map(|choice| PickerItem {
                value: choice.id.clone(),
                label: choice.id.clone(),
                detail: model_detail(choice),
                current: choice.id == current,
                tags: Vec::new(),
            })
            .collect();
        // The model in use is listed even when the provider doesn't list it.
        if !items.iter().any(|item| item.current) {
            items.push(PickerItem {
                value: current.to_string(),
                label: current.to_string(),
                detail: "in use".into(),
                current: true,
                tags: Vec::new(),
            });
        }
        let (views, lists) = match super::picks::rank(&choices) {
            Some(ranking) => {
                for (index, reasons) in &ranking.top {
                    items[*index].tags = reasons.clone();
                }
                let mut views = Vec::new();
                let mut lists = Vec::new();
                let top: Vec<usize> = ranking.top.iter().map(|(index, _)| *index).collect();
                for (view, list) in [
                    (ModelView::Top, top),
                    (ModelView::Smartest, ranking.smartest),
                    (ModelView::Cheapest, ranking.cheapest),
                    (ModelView::Context, ranking.context),
                    (ModelView::Popular, ranking.popular),
                ] {
                    if !list.is_empty() {
                        views.push(view);
                        lists.push((view, list));
                    }
                }
                views.push(ModelView::All);
                (views, lists)
            }
            None => (vec![ModelView::All], Vec::new()),
        };
        picker.view = views[0];
        picker.views = views;
        picker.lists = lists;
        picker.items = items;
        picker.note = note;
        picker.selected = 0;
        picker.loading = false;
    }

    fn fill_reasoning_picker(
        &mut self,
        model: &str,
        current: Option<String>,
        levels: Vec<String>,
        verified: bool,
    ) {
        if !self
            .picker
            .as_ref()
            .is_some_and(|p| p.kind == PickerKind::Reasoning && p.loading)
        {
            return;
        }
        if verified && levels.is_empty() {
            self.picker = None;
            self.push_notice(format!("{model} has no adjustable reasoning level."));
            return;
        }
        let item = |value: &str, is_current: bool| PickerItem {
            value: value.to_string(),
            label: value.to_string(),
            detail: level_blurb(value).to_string(),
            current: is_current,
            tags: Vec::new(),
        };
        let mut items = vec![item("off", current.is_none())];
        items.extend(
            levels
                .iter()
                .map(|level| item(level, current.as_deref() == Some(level.as_str()))),
        );
        if let Some(picker) = self.picker.as_mut() {
            picker.selected = items.iter().position(|i| i.current).unwrap_or(0);
            picker.items = items;
            picker.title = format!(" How much should {model} think? ");
            picker.note = (!verified).then(|| {
                "HiveMind can't tell which levels this model accepts, so your choice is sent as-is. \
                 If the provider rejects it, choose off."
                    .to_string()
            });
            picker.loading = false;
        }
    }

    /// Takes the highlighted row, closing the picker. `None` while still loading.
    pub(crate) fn apply_picker(&mut self) -> Option<PickerChoice> {
        let picker = self.picker.as_ref().filter(|p| !p.loading)?;
        let item = picker.chosen()?;
        let choice = match picker.kind {
            PickerKind::Model => PickerChoice::Model(item.value.clone()),
            PickerKind::Reasoning => {
                PickerChoice::Reasoning((item.value != "off").then(|| item.value.clone()))
            }
        };
        self.picker = None;
        Some(choice)
    }

    /// A click on a row selects and applies it.
    pub(crate) fn click_picker(&mut self, column: u16, row: u16) -> Option<PickerChoice> {
        let layout = self.layout.get();
        if !layout.picker_list.contains(Position::new(column, row)) {
            return None;
        }
        let picker = self.picker.as_mut()?;
        let index = picker.offset.get() + (row - layout.picker_list.y) as usize;
        if index >= picker.visible().len() {
            return None;
        }
        picker.selected = index;
        self.apply_picker()
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
        let step: isize = if down { 1 } else { -1 };
        if let Some(picker) = &mut self.picker {
            picker.move_by(step * SCROLL_ROWS);
            return;
        }
        if self.detail.is_some() {
            self.scroll_detail(step * SCROLL_ROWS);
            return;
        }
        let layout = self.layout.get();
        let pointer = Position::new(column, row);
        if layout.tools.contains(pointer) {
            let (offset, selected) = listview::scroll(
                &self.tool_heights(),
                self.tools_offset.get(),
                self.selected_tool,
                layout.tools_rows,
                step,
            );
            self.tools_offset.set(offset);
            self.selected_tool = selected;
        } else if layout.files.contains(pointer) {
            let (offset, selected) = listview::scroll(
                &vec![1; self.changed_files.len()],
                self.files_offset.get(),
                self.selected_file,
                layout.files_rows,
                step,
            );
            self.files_offset.set(offset);
            self.selected_file = selected;
        } else if layout.tasks.contains(pointer) {
            self.scroll_tasks(step);
        } else {
            self.scroll_conversation(step * SCROLL_ROWS);
        }
    }

    pub(crate) fn tool_heights(&self) -> Vec<usize> {
        self.tools.iter().map(tool_card_height).collect()
    }

    /// Left click: focus the panel there and select its row. True when the
    /// row was already selected, meaning "open it".
    pub(crate) fn click_at(&mut self, column: u16, row: u16) -> bool {
        if self.detail.is_some() {
            return false;
        }
        let layout = self.layout.get();
        let pointer = Position::new(column, row);
        let list_row = |area: Rect| row.saturating_sub(area.y + 1) as usize;
        if layout.tools.contains(pointer) {
            self.focus = Panel::Tools;
            let hit = listview::index_at(
                &self.tool_heights(),
                self.tools_offset.get(),
                list_row(layout.tools),
            );
            if let Some(index) = hit {
                let again = index == self.selected_tool;
                self.selected_tool = index;
                return again;
            }
        } else if layout.files.contains(pointer) {
            self.focus = Panel::Files;
            let hit = listview::index_at(
                &vec![1; self.changed_files.len()],
                self.files_offset.get(),
                list_row(layout.files),
            );
            if let Some(index) = hit {
                let again = index == self.selected_file;
                self.selected_file = index;
                return again;
            }
        } else if layout.tasks.contains(pointer) {
            self.focus = Panel::Tasks;
        } else if layout.conversation.contains(pointer) {
            self.focus = Panel::Conversation;
        }
        false
    }

    /// Tab order. Only the conversation exists while the side panel is hidden.
    pub(crate) fn cycle_focus(&mut self, backwards: bool) {
        const ALL: [Panel; 4] = [
            Panel::Conversation,
            Panel::Tasks,
            Panel::Tools,
            Panel::Files,
        ];
        if !self.layout.get().sidebar_shown {
            self.focus = Panel::Conversation;
            return;
        }
        let at = ALL.iter().position(|p| *p == self.focus).unwrap_or(0);
        let next = if backwards {
            at + ALL.len() - 1
        } else {
            at + 1
        };
        self.focus = ALL[next % ALL.len()];
    }

    /// One arrow-key step on the focused panel.
    pub(crate) fn step_focused(&mut self, steps: isize) {
        match self.focus {
            Panel::Conversation => self.scroll_conversation(steps * SCROLL_ROWS),
            Panel::Tasks => self.scroll_tasks(steps),
            Panel::Tools => self.scroll_tools(steps),
            Panel::Files => {
                if let Some(last) = self.changed_files.len().checked_sub(1) {
                    self.selected_file = self.selected_file.saturating_add_signed(steps).min(last);
                }
            }
        }
    }

    pub(crate) fn page_focused(&mut self, down: bool) {
        match self.focus {
            Panel::Conversation => self.page_conversation(down),
            _ => self.step_focused(if down { 5 } else { -5 }),
        }
    }

    /// Home/End on the focused panel.
    pub(crate) fn jump_focused(&mut self, to_end: bool) {
        match (self.focus, to_end) {
            (Panel::Conversation, true) => self.scroll_conversation_to_latest(),
            (Panel::Conversation, false) => self.scroll_conversation_to_top(),
            (_, true) => self.step_focused(isize::MAX / 2),
            (_, false) => self.step_focused(-(isize::MAX / 2)),
        }
    }

    /// Enter on an empty composer: open the focused tool or file.
    pub(crate) fn open_focused(&mut self, files: &dyn FileSource) {
        match self.focus {
            Panel::Tools => self.open_tool(self.selected_tool),
            Panel::Files => self.open_file(self.selected_file, files),
            Panel::Conversation | Panel::Tasks => {}
        }
    }

    fn open_tool(&mut self, index: usize) {
        if let Some(card) = self.tools.get(index) {
            self.selected_tool = index;
            self.detail = Some(Detail {
                kind: DetailKind::Tool(card.call_id.clone()),
                title: String::new(),
                lines: Vec::new(),
                scroll: 0,
            });
        }
    }

    fn open_file(&mut self, index: usize, files: &dyn FileSource) {
        let Some(file) = self.changed_files.get(index) else {
            return;
        };
        self.selected_file = index;
        self.detail = Some(Detail {
            kind: DetailKind::File(index),
            title: format!(" {}{} ", file.path, stats_label(file.stats)),
            lines: super::detail::file_lines(files.before(&file.path), files.now(&file.path)),
            scroll: 0,
        });
    }

    /// Ctrl+O: everything so far, every tool call expanded, opened at the end.
    pub(crate) fn open_transcript(&mut self) {
        self.detail = Some(Detail {
            kind: DetailKind::Transcript,
            title: " Transcript · every tool call expanded ".into(),
            lines: Vec::new(),
            scroll: usize::MAX / 2,
        });
    }

    /// `?` or `/help`: every key and command.
    pub(crate) fn open_help(&mut self) {
        self.detail = Some(Detail {
            kind: DetailKind::Help,
            title: " Keys and commands ".into(),
            lines: super::detail::help_lines(),
            scroll: 0,
        });
    }

    /// `/diff`: every file this session changed, one after another.
    pub(crate) fn open_all_changes(&mut self, files: &dyn FileSource) {
        let entries: Vec<_> = self
            .changed_files
            .iter()
            .map(|f| (f.path.clone(), files.before(&f.path), files.now(&f.path)))
            .collect();
        self.detail = Some(Detail {
            kind: DetailKind::Changes,
            title: format!(" Changes this session · {} file(s) ", entries.len()),
            lines: super::detail::changes_lines(&entries),
            scroll: 0,
        });
    }

    /// ←/→ inside a detail view: the previous or next tool or file.
    pub(crate) fn step_detail(&mut self, forward: bool, files: &dyn FileSource) {
        let Some(detail) = &self.detail else {
            return;
        };
        let step = |index: usize, len: usize| {
            if forward {
                (index + 1 < len).then_some(index + 1)
            } else {
                index.checked_sub(1)
            }
        };
        match detail.kind.clone() {
            DetailKind::Tool(id) => {
                if let Some(next) = self
                    .tools
                    .iter()
                    .position(|card| card.call_id == id)
                    .and_then(|index| step(index, self.tools.len()))
                {
                    self.open_tool(next);
                }
            }
            DetailKind::File(index) => {
                if let Some(next) = step(index, self.changed_files.len()) {
                    self.open_file(next, files);
                }
            }
            DetailKind::Changes | DetailKind::Transcript | DetailKind::Help => {}
        }
    }

    pub(crate) fn scroll_detail(&mut self, rows: isize) {
        let max = self.layout.get().detail_max_scroll;
        if let Some(detail) = &mut self.detail {
            detail.scroll = detail.scroll.min(max).saturating_add_signed(rows).min(max);
        }
    }

    pub(crate) fn page_detail(&mut self, down: bool) {
        let page = self.layout.get().detail_page.saturating_sub(1).max(1) as isize;
        self.scroll_detail(if down { page } else { -page });
    }

    /// Recounts each changed file's +/− lines. Cheap: the diff is size-capped.
    pub(crate) fn refresh_file_stats(&mut self, files: &dyn FileSource) {
        if !std::mem::take(&mut self.files_dirty) {
            return;
        }
        for file in &mut self.changed_files {
            file.stats = match (files.before(&file.path), files.now(&file.path)) {
                (Some(Some(before)), Some(now)) => {
                    let stats = crate::diff::stats(&crate::diff::diff(&before, &now));
                    Some((stats.added, stats.removed))
                }
                (Some(None), Some(now)) => Some((now.lines().count(), 0)),
                (Some(Some(before)), None) => Some((0, before.lines().count())),
                _ => None,
            };
        }
    }

    pub(crate) fn run_finished(&mut self, result: Result<(), String>) {
        if self.approval.is_none() {
            self.run_state = RunState::Idle;
            self.run_started = None;
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
                    kind: ConversationKind::Heading,
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
        self.last_reply.push_str(delta);
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
            let title = if kind == ConversationKind::User {
                "You"
            } else {
                "HiveMind"
            };
            self.conversation.push_back(ConversationLine {
                text: title.into(),
                kind: ConversationKind::Heading,
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
                    stats: None,
                });
            }
            self.files_dirty = true;
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
            self.tools_offset
                .set(self.tools_offset.get().saturating_sub(1));
        }
    }
}

/// "  +12 −3" for a title, or nothing when the counts are unknown.
pub(crate) fn stats_label(stats: Option<(usize, usize)>) -> String {
    stats.map_or(String::new(), |(added, removed)| {
        format!("  +{added} −{removed}")
    })
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
    fn command_names_resolve_by_exact_match_unique_prefix_or_ambiguity() {
        assert_eq!(resolve_command("/model"), Resolved::Command("/model"));
        assert_eq!(resolve_command("/rea"), Resolved::Command("/reasoning"));
        assert_eq!(resolve_command("/mod"), Resolved::Command("/model"));
        match resolve_command("/re") {
            Resolved::Ambiguous(names) => {
                assert!(
                    names.contains(&"/reasoning") && names.contains(&"/reviews"),
                    "{names:?}"
                )
            }
            other => panic!("expected an ambiguity, got {other:?}"),
        }
        assert_eq!(resolve_command("/zzz"), Resolved::Unknown);
        assert_eq!(resolve_command("/"), Resolved::Unknown);
        assert_eq!(resolve_command("model"), Resolved::Unknown);
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
