use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use super::app::{App, ConversationKind, RunState, TodoState, ToolState, View};

const BORDER: Color = Color::Rgb(83, 101, 119);
const MUTED: Color = Color::Rgb(159, 173, 189);
const ACCENT: Color = Color::Rgb(99, 208, 225);
const GREEN: Color = Color::Rgb(113, 239, 157);
const YELLOW: Color = Color::Rgb(250, 198, 79);
const RED: Color = Color::Rgb(246, 120, 111);

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let compact = area.width < 108 || area.height < 28;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(if app.composer.lines().count() > 2 {
                7
            } else {
                5
            }),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, app, chunks[0]);
    if compact || !app.sidebar_visible {
        render_main(frame, app, chunks[1]);
    } else {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
            .split(chunks[1]);
        render_main(frame, app, body[0]);
        render_sidebar(frame, app, body[1]);
    }
    render_composer(frame, app, chunks[2]);
    render_footer(frame, app, chunks[3], compact);

    if let Some(approval) = &app.approval {
        render_approval(frame, approval.command.as_str());
    } else if app.palette_visible {
        render_palette(frame);
    } else if app.details_visible {
        render_tool_details(frame, app);
    }
}

fn boxed(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(title)
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let state_style = match app.run_state {
        RunState::Idle => Style::default().fg(MUTED),
        RunState::Working => Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        RunState::WaitingForApproval => Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
    };
    let title = Line::from(vec![
        Span::styled(
            " HiveMind ",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled("│  ", Style::default().fg(BORDER)),
        Span::raw("workspace: "),
        Span::styled(shorten(&app.workspace, 44), Style::default().fg(ACCENT)),
        Span::raw("     model: "),
        Span::styled(shorten(&app.model, 28), Style::default().fg(Color::White)),
        Span::raw("     state: "),
        Span::styled(format!("● {}", app.run_state.label()), state_style),
    ]);
    frame.render_widget(Paragraph::new(title).block(boxed("")), area);
}

fn render_main(frame: &mut Frame, app: &App, area: Rect) {
    match app.view {
        View::Work => render_conversation(frame, app, area),
        View::Sessions => render_sessions(frame, app, area),
        View::Reviews => render_reviews(frame, app, area),
    }
}

fn render_conversation(frame: &mut Frame, app: &App, area: Rect) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let total = app.conversation.len();
    let visible = inner_height
        .saturating_add(app.conversation_scroll)
        .min(total);
    let start = total.saturating_sub(visible);
    let end = total.saturating_sub(app.conversation_scroll);
    let lines: Vec<Line<'static>> = app
        .conversation
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|line| match line.kind {
            ConversationKind::User => Line::from(Span::styled(
                line.text.clone(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            ConversationKind::Assistant => Line::from(Span::styled(
                line.text.clone(),
                Style::default().fg(Color::White),
            )),
            ConversationKind::Notice => {
                Line::from(Span::styled(line.text.clone(), Style::default().fg(MUTED)))
            }
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(boxed(" Conversation "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let tasks = app.todos.len().min(6) as u16;
    let changes = app.changed_files.len().min(5) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length((tasks + 3).max(4)),
            Constraint::Min(8),
            Constraint::Length((changes + 3).max(4)),
        ])
        .split(area);
    render_tasks(frame, app, chunks[0]);
    render_tools(frame, app, chunks[1]);
    render_changes(frame, app, chunks[2]);
}

fn render_tasks(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem<'static>> = if app.todos.is_empty() {
        vec![ListItem::new(Line::styled(
            "No task checklist yet",
            Style::default().fg(MUTED),
        ))]
    } else {
        app.todos
            .iter()
            .take(6)
            .map(|todo| {
                let (mark, style) = match todo.state {
                    TodoState::Completed => ("✓", Style::default().fg(GREEN)),
                    TodoState::InProgress => ("›", Style::default().fg(YELLOW)),
                    TodoState::Pending => ("○", Style::default().fg(MUTED)),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{mark} "), style),
                    Span::raw(todo.text.clone()),
                ]))
            })
            .collect()
    };
    frame.render_widget(List::new(items).block(boxed(" Tasks ")), area);
}

fn render_tools(frame: &mut Frame, app: &App, area: Rect) {
    let mut items = Vec::new();
    for (index, tool) in app.tools.iter().enumerate().rev().take(8) {
        let style = tool_style(tool.state);
        let marker = if index == app.selected_tool && app.details_visible {
            "›"
        } else {
            " "
        };
        items.push(ListItem::new(vec![
            Line::from(vec![
                Span::styled(format!("{marker} {} ", tool.state.symbol()), style),
                Span::styled(
                    tool.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {}", tool.state.label()), style),
            ]),
            Line::styled(shorten(&tool.call_id, 42), Style::default().fg(MUTED)),
        ]));
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::styled(
            "No tools have run",
            Style::default().fg(MUTED),
        )));
    }
    frame.render_widget(
        List::new(items).block(boxed(" Tools  [Tab] details ")),
        area,
    );
}

fn render_changes(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem<'static>> = if app.changed_files.is_empty() {
        vec![ListItem::new(Line::styled(
            "No files changed",
            Style::default().fg(MUTED),
        ))]
    } else {
        app.changed_files
            .iter()
            .take(5)
            .map(|file| {
                let (kind, color) = match file.kind {
                    harness_tools::FileChangeKind::Created => ("A", GREEN),
                    harness_tools::FileChangeKind::Modified => ("M", YELLOW),
                    harness_tools::FileChangeKind::Deleted => ("D", RED),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{kind}  "), Style::default().fg(color)),
                    Span::raw(shorten(&file.path, 44)),
                ]))
            })
            .collect()
    };
    frame.render_widget(List::new(items).block(boxed(" Changed files ")), area);
}

fn render_sessions(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![Line::styled(
        "Saved sessions for this workspace. Resume one from the command line with --resume <id>.",
        Style::default().fg(MUTED),
    )];
    for session in app.sessions.iter().take(20) {
        lines.push(Line::from(vec![
            Span::styled(
                &session.id,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  {} turn(s)  ${:.6}",
                session.turns, session.cost_usd
            )),
        ]));
        lines.push(Line::styled(
            shorten(&session.title, 100),
            Style::default().fg(Color::White),
        ));
    }
    if app.sessions.is_empty() {
        lines.push(Line::styled(
            "No saved sessions.",
            Style::default().fg(MUTED),
        ));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(boxed(" Sessions "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_reviews(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![Line::styled(
        "Saved reports created by `hivemind review`.",
        Style::default().fg(MUTED),
    )];
    for review in app.reviews.iter().take(10) {
        lines.push(Line::from(vec![
            Span::styled(
                review.review_id.0.clone(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  {} finding(s), {} high, {} critical",
                review.summary.findings_total, review.summary.high, review.summary.critical
            )),
        ]));
        for finding in review.findings.iter().take(3) {
            lines.push(Line::styled(
                format!("  • {:?}: {}", finding.severity, finding.title),
                Style::default().fg(Color::White),
            ));
        }
    }
    if app.reviews.is_empty() {
        lines.push(Line::styled(
            "No saved reviews.",
            Style::default().fg(MUTED),
        ));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(boxed(" Reviews "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_composer(frame: &mut Frame, app: &App, area: Rect) {
    let title = match app.view {
        View::Work => " Message HiveMind ",
        View::Sessions => " Message HiveMind (F1 for work) ",
        View::Reviews => " Message HiveMind (F1 for work) ",
    };
    let content = if app.composer.is_empty() {
        "Ask HiveMind to work on your code…".to_string()
    } else {
        app.composer.clone()
    };
    let style = if app.composer.is_empty() {
        Style::default().fg(MUTED)
    } else {
        Style::default().fg(Color::White)
    };
    frame.render_widget(
        Paragraph::new(content)
            .style(style)
            .block(boxed(title).border_style(Style::default().fg(ACCENT)))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect, compact: bool) {
    let text = if compact {
        "Enter send  Shift+Enter line  Ctrl+C interrupt  Ctrl+P commands"
    } else {
        "Enter send  Shift+Enter line  Ctrl+C interrupt  Ctrl+L clear  Ctrl+S panel  Tab details  F1 work  F2 sessions  F3 reviews  Ctrl+P commands"
    };
    let usage = if app.prompt_tokens + app.completion_tokens > 0 {
        format!(
            "   {} in / {} out · ${:.6}",
            app.prompt_tokens, app.completion_tokens, app.session_cost_usd
        )
    } else {
        String::new()
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(text, Style::default().fg(MUTED)),
            Span::styled(usage, Style::default().fg(ACCENT)),
        ])),
        area,
    );
}

fn render_approval(frame: &mut Frame, command: &str) {
    let area = centered_rect(72, 13, frame.area());
    frame.render_widget(Clear, area);
    let text = Text::from(vec![
        Line::styled(
            "Run command (approval required)",
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(command.to_string(), Style::default().fg(Color::White)),
        Line::raw(""),
        Line::styled(
            "[Y] Approve     [N] Deny     The command will run in the workspace.",
            Style::default().fg(MUTED),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(boxed(" Approval ").border_style(Style::default().fg(YELLOW)))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_palette(frame: &mut Frame) {
    let area = centered_rect(56, 15, frame.area());
    frame.render_widget(Clear, area);
    let text = Text::from(vec![
        Line::styled(
            "Command palette",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("F1  Work view"),
        Line::raw("F2  Saved sessions"),
        Line::raw("F3  Saved reviews"),
        Line::raw("Ctrl+L  Clear this screen"),
        Line::raw("Ctrl+S  Toggle side panel"),
        Line::raw("/model <id>  Change model"),
        Line::raw("/reasoning <level|off>, /budget <amount|off>, /web <on|off>"),
        Line::styled("Esc closes this panel", Style::default().fg(MUTED)),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(boxed(" Commands ").border_style(Style::default().fg(ACCENT)))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_tool_details(frame: &mut Frame, app: &App) {
    let Some(tool) = app.tools.get(app.selected_tool) else {
        return;
    };
    let area = centered_rect(76, 17, frame.area());
    frame.render_widget(Clear, area);
    let output = if tool.result.is_empty() {
        "No result yet.".to_string()
    } else {
        shorten(&tool.result, 1_600)
    };
    let text = Text::from(vec![
        Line::from(vec![
            Span::styled("Call: ", Style::default().fg(MUTED)),
            Span::styled(tool.call_id.clone(), Style::default().fg(ACCENT)),
        ]),
        Line::from(vec![
            Span::styled("Arguments: ", Style::default().fg(MUTED)),
            Span::raw(shorten(&tool.args, 700)),
        ]),
        Line::raw(""),
        Line::styled(
            "Result",
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Line::raw(output),
        Line::raw(""),
        Line::styled(
            "Tab closes details; Up/Down scroll the conversation.",
            Style::default().fg(MUTED),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                boxed(format!(" Tool: {} · {} ", tool.name, tool.state.label()))
                    .border_style(tool_style(tool.state)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn tool_style(state: ToolState) -> Style {
    match state {
        ToolState::Completed => Style::default().fg(GREEN),
        ToolState::WaitingForApproval => Style::default().fg(YELLOW),
        ToolState::Denied | ToolState::Failed | ToolState::TimedOut => Style::default().fg(RED),
        ToolState::Running => Style::default().fg(ACCENT),
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = area
        .width
        .saturating_mul(percent_x)
        .saturating_div(100)
        .max(30);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

fn shorten(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    if count <= limit {
        value.to_string()
    } else {
        let head: String = value.chars().take(limit.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn narrow_layout_and_unicode_render_without_panicking() {
        let mut app = App::new("世界".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.composer = "नमस्ते\nsecond line".into();
        app.apply(super::super::events::UiEvent::AssistantDelta(
            "Hello 世界".into(),
        ));
        let backend = TestBackend::new(48, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
    }

    #[test]
    fn approval_overlay_renders_on_a_small_screen() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.approval = Some(super::super::app::Approval {
            request_id: "a".into(),
            command: "cargo test".into(),
        });
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
    }

    #[test]
    fn layout_can_be_redrawn_after_a_resize() {
        let app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        let backend = TestBackend::new(140, 42);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 52, 18))
            .unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
    }
}
