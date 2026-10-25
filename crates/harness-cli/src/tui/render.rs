use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use super::app::{App, ConversationKind, RunState, TodoState, ToolState, View, slash_matches};

const BORDER: Color = Color::Rgb(83, 101, 119);
const MUTED: Color = Color::Rgb(159, 173, 189);
const ACCENT: Color = Color::Rgb(99, 208, 225);
const GREEN: Color = Color::Rgb(113, 239, 157);
const YELLOW: Color = Color::Rgb(250, 198, 79);
const RED: Color = Color::Rgb(246, 120, 111);

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    // Hidden panels must not keep catching the mouse wheel.
    let mut layout = app.layout.get();
    layout.conversation = Rect::default();
    layout.tools = Rect::default();
    app.layout.set(layout);

    let area = frame.area();
    let compact = area.width < 108 || area.height < 28;
    let composer_rows = wrap_rows(&app.composer, area.width.saturating_sub(2) as usize)
        .len()
        .clamp(3, 6) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(composer_rows + 2),
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

    if app.approval.is_none() && !app.palette_visible {
        render_slash_hints(frame, app, chunks[2]);
    }
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
    let lines: Vec<Line<'static>> = app
        .conversation
        .iter()
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
    let title = if app.conversation_top.is_some() {
        " Conversation · scrolled up · End for latest "
    } else {
        " Conversation "
    };
    let block = boxed(title);
    let inner = block.inner(area);
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    // Counted in wrapped rows, so long lines can't push the newest text out of view.
    let total = paragraph.line_count(inner.width);
    let page = inner.height as usize;
    let max_top = total.saturating_sub(page);
    let top = app.conversation_top.map_or(max_top, |top| top.min(max_top));

    let mut layout = app.layout.get();
    layout.conversation = area;
    layout.conversation_page = page;
    layout.conversation_max_top = max_top;
    app.layout.set(layout);

    frame.render_widget(
        paragraph
            .block(block)
            .scroll((top.min(u16::MAX as usize) as u16, 0)),
        area,
    );
    if max_top > 0 {
        let mut state = ScrollbarState::new(max_top + 1)
            .viewport_content_length(page)
            .position(top);
        render_scrollbar(frame, area, &mut state);
    }
}

fn render_scrollbar(frame: &mut Frame, area: Rect, state: &mut ScrollbarState) {
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_style(Style::default().fg(ACCENT))
            .track_style(Style::default().fg(BORDER)),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        state,
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
    let mut layout = app.layout.get();
    layout.tools = area;
    app.layout.set(layout);

    let block = boxed(format!(" Tools ({})  Tab details ", app.tools.len()));
    if app.tools.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No tools have run",
                Style::default().fg(MUTED),
            ))
            .block(block),
            area,
        );
        return;
    }
    let detail_width = (area.width as usize).saturating_sub(7);
    let items: Vec<ListItem<'static>> = app
        .tools
        .iter()
        .map(|tool| {
            let style = tool_style(tool.state);
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("{} ", tool.state.symbol()), style),
                    Span::styled(
                        tool.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {}", tool.state.label()), style),
                ]),
                Line::styled(
                    format!("  {}", shorten(&tool_summary(&tool.args), detail_width)),
                    Style::default().fg(MUTED),
                ),
            ])
        })
        .collect();
    let selected = app.selected_tool.min(app.tools.len().saturating_sub(1));
    // Stateful so ratatui keeps the selected call scrolled into view.
    let mut state = ListState::default().with_selected(Some(selected));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::Rgb(38, 50, 62)))
        .highlight_symbol("›");
    frame.render_stateful_widget(list, area, &mut state);
    // Each card is two rows.
    if app.tools.len() * 2 > area.height.saturating_sub(2) as usize {
        let mut bar = ScrollbarState::new(app.tools.len()).position(selected);
        render_scrollbar(frame, area, &mut bar);
    }
}

/// The argument that identifies a call: its command, path, or query.
fn tool_summary(args: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(args) {
        for key in ["command", "path", "pattern", "query", "url"] {
            if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
                return text.lines().next().unwrap_or_default().to_string();
            }
        }
    }
    args.split_whitespace().collect::<Vec<_>>().join(" ")
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
        View::Sessions | View::Reviews => " Message HiveMind  (/work to go back) ",
    };
    let block = boxed(title).border_style(Style::default().fg(ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Approval and the palette take the keyboard, so no cursor there.
    let wants_cursor = app.approval.is_none() && !app.palette_visible;

    if app.composer.is_empty() {
        frame.render_widget(
            Paragraph::new("Ask HiveMind to work on your code…   type / for commands")
                .style(Style::default().fg(MUTED)),
            inner,
        );
        if wants_cursor {
            frame.set_cursor_position((inner.x, inner.y));
        }
        return;
    }

    // Hard-wrapped and bottom-scrolled so the cursor lands where the next character appears.
    let rows = wrap_rows(&app.composer, inner.width as usize);
    let start = rows.len().saturating_sub(inner.height as usize);
    let visible: Vec<Line<'static>> = rows[start..].iter().map(|r| Line::raw(r.clone())).collect();
    frame.render_widget(
        Paragraph::new(Text::from(visible)).style(Style::default().fg(Color::White)),
        inner,
    );
    if wants_cursor && let Some(last) = rows.last() {
        let col = display_width(last) as u16;
        let row = (rows.len() - start - 1) as u16;
        frame.set_cursor_position((
            inner.x + col.min(inner.width.saturating_sub(1)),
            inner.y + row,
        ));
    }
}

/// Live list of matching commands while a `/` name is being typed.
fn render_slash_hints(frame: &mut Frame, app: &App, composer: Rect) {
    let matches = slash_matches(&app.composer);
    if matches.is_empty() {
        return;
    }
    let shown = matches.len().min(8);
    let height = shown as u16 + 2;
    if composer.y < height {
        return;
    }
    let area = Rect::new(
        composer.x + 1,
        composer.y - height,
        composer.width.saturating_sub(2).min(54),
        height,
    );
    let items: Vec<ListItem<'static>> = matches
        .iter()
        .take(shown)
        .map(|(name, what)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{name:<12}"),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(*what, Style::default().fg(MUTED)),
            ]))
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items)
            .block(boxed(" Commands  Tab completes ").border_style(Style::default().fg(ACCENT))),
        area,
    );
}

fn char_width(ch: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*ch.encode_utf8(&mut buf)).width()
}

fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// Hard-wraps at `width` columns; a full last row gets an empty one for the cursor.
fn wrap_rows(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for ch in line.chars() {
            let w = char_width(ch);
            if used + w > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(ch);
            used += w;
        }
        rows.push(row);
    }
    if rows.last().is_some_and(|r| display_width(r) >= width) {
        rows.push(String::new());
    }
    rows
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect, compact: bool) {
    let text = if compact {
        "Enter send  / commands  ↑↓ scroll  End latest  Ctrl+C stop"
    } else {
        "Enter send  / commands  ↑↓ PgUp PgDn scroll  End latest  Shift+↑↓ tools  Tab details  Shift+drag select  Ctrl+C stop"
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
        Line::raw("F1 / /work      Work view"),
        Line::raw("F2 / /sessions  Saved sessions"),
        Line::raw("F3 / /reviews   Saved reviews"),
        Line::raw("Ctrl+L  Clear this screen"),
        Line::raw("Ctrl+S  Toggle side panel"),
        Line::raw("Type / in the message box for every command"),
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
            "Tab next · Shift+Tab previous · Esc close",
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

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_cursor_sits_right_after_the_typed_text() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.composer = "hello".into();
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        // Composer box starts at column 0, so text begins at 1; "hello" is 5 wide.
        assert_eq!(cursor.x, 6);
    }

    #[test]
    fn a_full_row_moves_the_cursor_to_the_next_one() {
        assert_eq!(
            wrap_rows("abcd", 4),
            vec!["abcd".to_string(), String::new()]
        );
        assert_eq!(
            wrap_rows("abcdef", 4),
            vec!["abcd".to_string(), "ef".to_string()]
        );
        assert_eq!(wrap_rows("ab\ncd", 10).len(), 2);
    }

    #[test]
    fn the_tools_list_scrolls_to_an_old_selected_call() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        for i in 0..30 {
            app.apply(super::super::events::UiEvent::ToolStarted {
                call_id: format!("c{i}"),
                name: "read_file".into(),
                args: format!(r#"{{"path":"file_{i:02}.rs"}}"#),
            });
        }
        app.selected_tool = 2;
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("file_02.rs"),
            "selected call must be scrolled into view"
        );
        assert!(
            !text.contains("call_"),
            "opaque call ids should not be shown"
        );
    }

    #[test]
    fn typing_a_slash_lists_matching_commands() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.composer = "/se".into();
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("/sessions"));
        assert!(!text.contains("/model"), "only matches are listed");
    }

    fn stream_lines(app: &mut App, range: std::ops::Range<usize>) {
        for i in range {
            app.apply(super::super::events::UiEvent::AssistantDelta(format!(
                "line {i:03}\n"
            )));
        }
    }

    fn drawn(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        screen(&terminal)
    }

    #[test]
    fn the_newest_reply_stays_visible_when_long_lines_wrap() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        for i in 0..40 {
            let long = format!("row {i:02} {}\n", "words that wrap ".repeat(14));
            app.apply(super::super::events::UiEvent::AssistantDelta(long));
        }
        app.apply(super::super::events::UiEvent::AssistantDelta(
            "LAST-LINE-MARKER".into(),
        ));
        assert!(drawn(&app).contains("LAST-LINE-MARKER"));
    }

    #[test]
    fn scrolling_up_reveals_older_text_and_holds_still_while_output_streams() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        stream_lines(&mut app, 0..100);
        let latest = drawn(&app);
        assert!(latest.contains("line 099") && !latest.contains("line 020"));

        app.scroll_conversation(-60);
        let scrolled = drawn(&app);
        assert!(scrolled.contains("line 020") && !scrolled.contains("line 099"));
        assert!(scrolled.contains("End for latest"));

        stream_lines(&mut app, 100..110);
        let still = drawn(&app);
        assert!(
            still.contains("line 020") && !still.contains("line 109"),
            "new output must not move a scrolled-up view"
        );

        app.scroll_conversation_to_latest();
        assert!(drawn(&app).contains("line 109"));
    }

    #[test]
    fn scrolling_is_clamped_and_reaching_the_bottom_follows_again() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        stream_lines(&mut app, 0..100);
        drawn(&app);
        app.scroll_conversation(-10_000);
        assert_eq!(app.conversation_top, Some(0));
        assert!(drawn(&app).contains("line 000"));
        app.scroll_conversation(10_000);
        assert_eq!(app.conversation_top, None);
    }

    #[test]
    fn sending_a_message_jumps_back_to_the_latest() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        stream_lines(&mut app, 0..100);
        drawn(&app);
        app.page_conversation(false);
        assert!(app.conversation_top.is_some());
        app.submitted("second question");
        assert_eq!(app.conversation_top, None);
        assert!(drawn(&app).contains("second question"));
    }

    #[test]
    fn the_wheel_scrolls_the_panel_under_the_pointer() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        stream_lines(&mut app, 0..100);
        for i in 0..30 {
            app.apply(super::super::events::UiEvent::ToolStarted {
                call_id: format!("c{i}"),
                name: "read_file".into(),
                args: format!(r#"{{"path":"file_{i:02}.rs"}}"#),
            });
        }
        drawn(&app);
        let layout = app.layout.get();

        app.scroll_at(layout.tools.x + 2, layout.tools.y + 2, false);
        assert_eq!(app.selected_tool, 28);
        assert_eq!(
            app.conversation_top, None,
            "tools wheel must not scroll the chat"
        );

        app.scroll_at(layout.conversation.x + 2, layout.conversation.y + 2, false);
        assert!(app.conversation_top.is_some());
        assert_eq!(app.selected_tool, 28);
    }

    #[test]
    fn a_hidden_side_panel_stops_catching_the_wheel() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        drawn(&app);
        assert_ne!(app.layout.get().tools, Rect::default());
        app.sidebar_visible = false;
        drawn(&app);
        assert_eq!(app.layout.get().tools, Rect::default());
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
