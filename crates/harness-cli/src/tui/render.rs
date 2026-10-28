use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use super::app::{
    App, ConversationKind, CostStatus, Panel, PickerKind, RunState, TodoState, ToolCard, ToolState,
    View, failure_reason, slash_matches, tool_verb,
};
use super::{listview, markdown};

pub(crate) const BORDER: Color = Color::Rgb(83, 101, 119);
pub(crate) const MUTED: Color = Color::Rgb(159, 173, 189);
pub(crate) const ACCENT: Color = Color::Rgb(99, 208, 225);
pub(crate) const GREEN: Color = Color::Rgb(113, 239, 157);
pub(crate) const YELLOW: Color = Color::Rgb(250, 198, 79);
pub(crate) const RED: Color = Color::Rgb(246, 120, 111);

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    // Hidden panels must not keep catching the mouse or Tab focus.
    let mut layout = app.layout.get();
    layout.conversation = Rect::default();
    layout.tools = Rect::default();
    layout.tasks = Rect::default();
    layout.files = Rect::default();
    layout.sidebar_shown = false;
    layout.picker = Rect::default();
    layout.picker_list = Rect::default();
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
    // Over the body only, so the message box and footer stay usable.
    super::detail::render_detail(frame, app, chunks[1]);
    render_composer(frame, app, chunks[2]);
    render_footer(frame, app, chunks[3], compact);

    if app.approval.is_none() && !app.palette_visible && app.picker.is_none() {
        render_slash_hints(frame, app, chunks[2]);
    }
    render_picker(frame, app);
    if let Some(approval) = &app.approval {
        render_approval(frame, approval.command.as_str());
    } else if app.palette_visible {
        render_palette(frame);
    }
}

pub(crate) fn boxed(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(title)
}

/// A side or main panel; the focused one gets the accent border.
fn panel(app: &App, which: Panel, title: impl Into<Line<'static>>) -> Block<'static> {
    let block = boxed(title);
    if app.focus == which && app.detail.is_none() {
        block.border_style(Style::default().fg(ACCENT))
    } else {
        block
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let state_style = match app.run_state {
        RunState::Idle => Style::default().fg(MUTED),
        RunState::Working => Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        RunState::WaitingForApproval => Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
    };
    let mut spans = vec![
        Span::styled(
            " HiveMind ",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled("│  ", Style::default().fg(BORDER)),
        Span::raw("workspace: "),
        Span::styled(
            shorten(&display_workspace(&app.workspace), 44),
            Style::default().fg(ACCENT),
        ),
        Span::raw("     "),
    ];
    if !app.connection.is_empty() {
        spans.push(Span::styled(
            format!("{} · ", shorten(&app.connection, 30)),
            Style::default().fg(MUTED),
        ));
    }
    spans.push(Span::raw("model: "));
    spans.push(Span::styled(
        shorten(&app.model, 28),
        Style::default().fg(Color::White),
    ));
    if let Some(level) = &app.reasoning {
        spans.push(Span::styled(
            format!(" · reasoning {level}"),
            Style::default().fg(MUTED),
        ));
    }
    if let Some(percent) = context_percent(app) {
        let color = match percent {
            0..60 => MUTED,
            60..85 => YELLOW,
            _ => RED,
        };
        spans.push(Span::styled(
            format!("     ctx {percent}%"),
            Style::default().fg(color),
        ));
    }
    spans.push(Span::raw("     state: "));
    spans.push(Span::styled(
        format!("● {}", app.run_state.label()),
        state_style,
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)).block(boxed("")), area);
}

/// How full the model's context was on the latest request. Only shown for
/// models with a known window; a guessed one would mislead.
fn context_percent(app: &App) -> Option<u64> {
    let window = harness_config::lookup_model(&app.model)?.context_window;
    (window > 0 && app.last_prompt_tokens > 0)
        .then(|| (app.last_prompt_tokens * 100 / window).min(100))
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The message box title while a run is going: what is happening, for how long.
fn activity_title(app: &App, width: u16) -> Option<Line<'static>> {
    let activity = app.activity()?;
    let elapsed = app.run_started.map(|t| t.elapsed()).unwrap_or_default();
    let frame = SPINNER[(elapsed.as_millis() / 100) as usize % SPINNER.len()];
    let tail = format!(" · {}s · Esc to stop ", elapsed.as_secs());
    let room = (width as usize).saturating_sub(display_width(&tail) + 8);
    Some(Line::from(vec![
        Span::styled(format!(" {frame} "), Style::default().fg(GREEN)),
        Span::styled(shorten(&activity, room), Style::default().fg(Color::White)),
        Span::styled(tail, Style::default().fg(MUTED)),
    ]))
}

fn render_main(frame: &mut Frame, app: &App, area: Rect) {
    match app.view {
        View::Work => render_conversation(frame, app, area),
        View::Sessions => render_sessions(frame, app, area),
        View::Reviews => render_reviews(frame, app, area),
    }
}

/// The conversation as drawn. `expand_tools` inlines each call's full
/// request and result, for the transcript view.
pub(crate) fn conversation_lines(app: &App, expand_tools: bool) -> Vec<Line<'static>> {
    let cards: std::collections::HashMap<&str, &ToolCard> = app
        .tools
        .iter()
        .map(|card| (card.call_id.as_str(), card))
        .collect();
    let mut in_code = false;
    let mut out = Vec::with_capacity(app.conversation.len());
    for line in &app.conversation {
        match line.kind {
            ConversationKind::Heading => {
                in_code = false;
                let color = if line.text == "You" { ACCENT } else { GREEN };
                out.push(Line::styled(
                    format!("▌{}", line.text),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            }
            ConversationKind::User => out.push(Line::styled(
                line.text.clone(),
                Style::default().fg(Color::White),
            )),
            ConversationKind::Assistant => {
                out.push(markdown::render_line(&line.text, &mut in_code))
            }
            ConversationKind::Notice => {
                out.push(Line::styled(line.text.clone(), Style::default().fg(MUTED)))
            }
            ConversationKind::Tool => {
                // Trimmed from the tool list long ago: nothing left to show.
                let Some(card) = cards.get(line.text.as_str()) else {
                    continue;
                };
                let style = tool_style(card.state);
                out.push(Line::from(vec![
                    Span::styled(format!("  {} ", card.state.symbol()), style),
                    Span::styled(
                        tool_verb(&card.name).to_string(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {}", card.summary), Style::default().fg(MUTED)),
                ]));
                if expand_tools {
                    out.extend(super::detail::tool_lines(card).into_iter().map(indented));
                    out.push(Line::raw(""));
                } else if let Some(reason) = failure_reason(card) {
                    out.push(Line::styled(
                        format!("    ↳ {reason}"),
                        Style::default().fg(RED),
                    ));
                }
            }
        }
    }
    out
}

fn indented(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::styled("    │ ", Style::default().fg(BORDER))];
    let style = line.style;
    spans.extend(line.spans);
    Line::from(spans).style(style)
}

fn render_conversation(frame: &mut Frame, app: &App, area: Rect) {
    let lines = conversation_lines(app, false);
    let title = if app.conversation_top.is_some() {
        " Conversation · scrolled up · End for latest "
    } else {
        " Conversation "
    };
    let block = panel(app, Panel::Conversation, title);
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

pub(crate) fn render_scrollbar(frame: &mut Frame, area: Rect, state: &mut ScrollbarState) {
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
    let mut layout = app.layout.get();
    layout.sidebar_shown = true;
    app.layout.set(layout);
    // Tasks and files each take a share; past it their lists scroll.
    let tasks = (app.todos.len().max(1) as u16 + 2).min((area.height * 2 / 5).max(3));
    let files = (app.changed_files.len().max(1) as u16 + 2).min((area.height / 4).max(3));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tasks),
            Constraint::Min(6),
            Constraint::Length(files),
        ])
        .split(area);
    render_tasks(frame, app, chunks[0]);
    render_tools(frame, app, chunks[1]);
    render_changes(frame, app, chunks[2]);
}

fn render_tasks(frame: &mut Frame, app: &App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let max_offset = app.todos.len().saturating_sub(visible);
    let offset = app.tasks_offset.min(max_offset);
    let mut layout = app.layout.get();
    layout.tasks = area;
    layout.tasks_max_offset = max_offset;
    app.layout.set(layout);

    let done = app
        .todos
        .iter()
        .filter(|todo| todo.state == TodoState::Completed)
        .count();
    let title = if app.todos.is_empty() {
        " Tasks ".to_string()
    } else {
        format!(" Tasks {done}/{} ", app.todos.len())
    };
    let items: Vec<ListItem<'static>> = if app.todos.is_empty() {
        vec![ListItem::new(Line::styled(
            "No task checklist yet",
            Style::default().fg(MUTED),
        ))]
    } else {
        app.todos
            .iter()
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
    let mut state = ListState::default().with_offset(offset);
    let block = panel(app, Panel::Tasks, title);
    frame.render_stateful_widget(List::new(items).block(block), area, &mut state);
    if max_offset > 0 {
        let mut bar = ScrollbarState::new(max_offset + 1)
            .viewport_content_length(visible)
            .position(offset);
        render_scrollbar(frame, area, &mut bar);
    }
}

/// Highlights the selected row only while its panel has focus.
fn selectable_list<'a>(items: Vec<ListItem<'a>>, block: Block<'a>, focused: bool) -> List<'a> {
    let list = List::new(items).block(block);
    if focused {
        list.highlight_style(Style::default().bg(Color::Rgb(38, 50, 62)))
            .highlight_symbol("›")
    } else {
        list.highlight_symbol(" ")
    }
}

fn render_tools(frame: &mut Frame, app: &App, area: Rect) {
    let rows = area.height.saturating_sub(2) as usize;
    let mut layout = app.layout.get();
    layout.tools = area;
    layout.tools_rows = rows;
    app.layout.set(layout);

    let focused = app.focus == Panel::Tools;
    let hint = if focused {
        "· ↑↓ select · Enter details "
    } else {
        ""
    };
    let block = panel(
        app,
        Panel::Tools,
        format!(" Tools {} {hint}", app.tools.len()),
    );
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
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(format!("{} ", tool.state.symbol()), style),
                    Span::styled(
                        tool.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {}", tool.state.label()), style),
                ]),
                Line::styled(
                    format!("  {}", shorten(&tool.summary, detail_width)),
                    Style::default().fg(MUTED),
                ),
            ];
            // A failure says why right on the card.
            if let Some(reason) = failure_reason(tool) {
                lines.push(Line::styled(
                    format!("  {}", shorten(reason, detail_width)),
                    Style::default().fg(RED),
                ));
            }
            ListItem::new(lines)
        })
        .collect();
    let heights = app.tool_heights();
    let selected = app.selected_tool.min(app.tools.len() - 1);
    let offset = listview::offset_showing(&heights, app.tools_offset.get(), selected, rows);
    app.tools_offset.set(offset);
    let mut state = ListState::default()
        .with_offset(offset)
        .with_selected(Some(selected));
    frame.render_stateful_widget(selectable_list(items, block, focused), area, &mut state);
    let max = listview::max_offset(&heights, rows);
    if max > 0 {
        let mut bar = ScrollbarState::new(max + 1).position(offset.min(max));
        render_scrollbar(frame, area, &mut bar);
    }
}

fn render_changes(frame: &mut Frame, app: &App, area: Rect) {
    let rows = area.height.saturating_sub(2) as usize;
    let mut layout = app.layout.get();
    layout.files = area;
    layout.files_rows = rows;
    app.layout.set(layout);

    let focused = app.focus == Panel::Files;
    let count = app.changed_files.len();
    let title = match (count, focused) {
        (0, _) => " Changed files ".to_string(),
        (_, true) => format!(" Changed files {count} · Enter diff "),
        (_, false) => format!(" Changed files {count} "),
    };
    let block = panel(app, Panel::Files, title);
    if count == 0 {
        frame.render_widget(
            Paragraph::new(Line::styled("No files changed", Style::default().fg(MUTED)))
                .block(block),
            area,
        );
        return;
    }
    let width = area.width.saturating_sub(5) as usize;
    let items: Vec<ListItem<'static>> = app
        .changed_files
        .iter()
        .map(|file| {
            let (kind, color) = match file.kind {
                harness_tools::FileChangeKind::Created => ("A", GREEN),
                harness_tools::FileChangeKind::Modified => ("M", YELLOW),
                harness_tools::FileChangeKind::Deleted => ("D", RED),
            };
            let mut spans = vec![Span::styled(format!("{kind} "), Style::default().fg(color))];
            match file.stats {
                Some((added, removed)) => {
                    let (plus, minus) = (format!(" +{added}"), format!(" −{removed}"));
                    let room =
                        width.saturating_sub(display_width(&plus) + display_width(&minus) + 2);
                    let path = shorten(&file.path, room);
                    let pad = room.saturating_sub(display_width(&path));
                    spans.push(Span::raw(format!("{path}{}", " ".repeat(pad))));
                    spans.push(Span::styled(plus, Style::default().fg(GREEN)));
                    spans.push(Span::styled(minus, Style::default().fg(RED)));
                }
                None => spans.push(Span::raw(shorten(&file.path, width.saturating_sub(2)))),
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let heights = vec![1; count];
    let selected = app.selected_file.min(count - 1);
    let offset = listview::offset_showing(&heights, app.files_offset.get(), selected, rows);
    app.files_offset.set(offset);
    let mut state = ListState::default()
        .with_offset(offset)
        .with_selected(Some(selected));
    frame.render_stateful_widget(selectable_list(items, block, focused), area, &mut state);
    let max = listview::max_offset(&heights, rows);
    if max > 0 {
        let mut bar = ScrollbarState::new(max + 1)
            .viewport_content_length(rows)
            .position(offset.min(max));
        render_scrollbar(frame, area, &mut bar);
    }
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
    let title = match (activity_title(app, area.width), app.view) {
        (Some(activity), _) => activity,
        (None, View::Work) => " Message HiveMind ".into(),
        (None, View::Sessions | View::Reviews) => " Message HiveMind  (/work to go back) ".into(),
    };
    let block = boxed(title).border_style(Style::default().fg(ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Approval and the palette take the keyboard, so no cursor there.
    let wants_cursor = app.approval.is_none() && !app.palette_visible && app.picker.is_none();

    if app.composer.is_empty() {
        // Starts one column in, so the cursor doesn't cover its first letter.
        frame.render_widget(
            Paragraph::new("Ask HiveMind to work on your code…   type / for commands")
                .style(Style::default().fg(MUTED)),
            inner.inner(Margin {
                vertical: 0,
                horizontal: 1,
            }),
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
    let text = match (app.detail.is_some(), compact) {
        (true, _) => "↑↓ PgUp PgDn scroll  ←→ previous/next  Esc close",
        (false, true) => "Tab panel  ↑↓ scroll  Enter open  ? help",
        (false, false) => {
            "Enter send  Tab panel  ↑↓ scroll  Enter open  Ctrl+O transcript  Esc stop  ? help"
        }
    };
    // Usage gets its space first; the key hints are what gets clipped.
    let usage = usage_summary(app);
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(display_width(&usage) as u16),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(MUTED))),
        parts[0],
    );
    frame.render_widget(
        Paragraph::new(Span::styled(usage, Style::default().fg(ACCENT))),
        parts[1],
    );
}

/// Session tokens and spend. An unknown price is shown as unknown, never as $0.
fn usage_summary(app: &App) -> String {
    if app.session_prompt_tokens + app.session_completion_tokens == 0 {
        return String::new();
    }
    let tokens = format!(
        "{} in · {} out",
        compact_count(app.session_prompt_tokens),
        compact_count(app.session_completion_tokens)
    );
    let cost = match app.cost_status {
        CostStatus::Local => "local model".to_string(),
        CostStatus::Known => format!("${:.4}", app.session_cost_usd),
        CostStatus::Partial if app.session_cost_usd > 0.0 => {
            format!("≥ ${:.4} (some prices unknown)", app.session_cost_usd)
        }
        CostStatus::Partial => "cost n/a for this model".to_string(),
    };
    format!("   {tokens} · {cost}")
}

fn compact_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
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

pub(crate) fn tool_style(state: ToolState) -> Style {
    match state {
        ToolState::Completed => Style::default().fg(GREEN),
        ToolState::WaitingForApproval => Style::default().fg(YELLOW),
        ToolState::Denied | ToolState::Failed | ToolState::TimedOut => Style::default().fg(RED),
        ToolState::Running => Style::default().fg(ACCENT),
    }
}

/// Windows hands back paths as `\\?\C:\...`; show the one a person would type.
fn display_workspace(path: &str) -> String {
    match path.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => rest.to_string(),
        },
        None => path.to_string(),
    }
}

/// The `/model` and `/reasoning` list: a title, an optional note and filter,
/// then the rows, scrolled so the highlighted one stays in view.
fn render_picker(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };
    let area = frame.area();
    let rows_wanted = picker.items.len().clamp(3, 14) as u16;
    let note_rows = picker.note.as_ref().map_or(0, |note| {
        let width = (area.width * 72 / 100).saturating_sub(4).max(10) as usize;
        wrap_rows(note, width).len().min(3) as u16
    });
    let filter_row = u16::from(picker.kind == PickerKind::Model);
    let height = (rows_wanted + note_rows + filter_row + 2).min(area.height);
    let rect = centered_rect(72, height, area);
    let hint = match picker.kind {
        PickerKind::Model => " ↑↓ choose · Enter select · type to filter · Esc cancel ",
        PickerKind::Reasoning => " ↑↓ choose · Enter select · Esc cancel ",
    };
    frame.render_widget(Clear, rect);
    let block = boxed(picker.title.clone())
        .border_style(Style::default().fg(ACCENT))
        .title_bottom(hint);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(note_rows),
            Constraint::Length(filter_row),
            Constraint::Min(1),
        ])
        .split(inner);
    if let Some(note) = &picker.note {
        frame.render_widget(
            Paragraph::new(note.clone())
                .style(Style::default().fg(YELLOW))
                .wrap(Wrap { trim: true }),
            parts[0],
        );
    }
    if filter_row == 1 {
        let shown = picker.visible().len();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Filter: ", Style::default().fg(MUTED)),
                Span::raw(picker.filter.clone()),
                Span::styled("▌", Style::default().fg(ACCENT)),
                Span::styled(
                    format!("   {shown} of {}", picker.items.len()),
                    Style::default().fg(MUTED),
                ),
            ])),
            parts[1],
        );
    }

    let list_area = parts[2];
    let mut layout = app.layout.get();
    layout.picker = rect;
    layout.picker_list = list_area;
    app.layout.set(layout);

    let visible = picker.visible();
    if picker.loading || visible.is_empty() {
        let text = if picker.loading {
            "Loading…".to_string()
        } else {
            format!("Nothing matches \"{}\".", picker.filter)
        };
        frame.render_widget(
            Paragraph::new(Line::styled(text, Style::default().fg(MUTED))),
            list_area,
        );
        return;
    }
    let width = list_area.width as usize;
    let label_width = visible
        .iter()
        .map(|&i| display_width(&picker.items[i].label))
        .max()
        .unwrap_or(0)
        .min(width / 2);
    let items: Vec<ListItem<'static>> = visible
        .iter()
        .map(|&index| {
            let item = &picker.items[index];
            let marker = if item.current { "● " } else { "  " };
            let label = shorten(&item.label, label_width);
            let pad = " ".repeat(label_width.saturating_sub(display_width(&label)) + 2);
            let detail_room = width.saturating_sub(2 + label_width + 2 + 2);
            ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(GREEN)),
                Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(pad),
                Span::styled(
                    shorten(&item.detail, detail_room),
                    Style::default().fg(MUTED),
                ),
            ]))
        })
        .collect();
    let rows = list_area.height as usize;
    let heights = vec![1; visible.len()];
    let selected = picker.selected.min(visible.len() - 1);
    let offset = listview::offset_showing(&heights, picker.offset.get(), selected, rows);
    picker.offset.set(offset);
    let mut state = ListState::default()
        .with_offset(offset)
        .with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().bg(Color::Rgb(38, 50, 62)))
            .highlight_symbol(""),
        list_area,
        &mut state,
    );
    let max = listview::max_offset(&heights, rows);
    if max > 0 {
        let mut bar = ScrollbarState::new(max + 1)
            .viewport_content_length(rows)
            .position(offset.min(max));
        render_scrollbar(frame, rect, &mut bar);
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
    use harness_tools::ToolStatus;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::app::PickerChoice;
    use super::super::app::{DetailKind, FileSource};
    use super::super::events::{ModelChoice, UiEvent};
    use super::*;

    #[derive(Default)]
    struct FakeFiles(std::collections::HashMap<String, (Option<Option<String>>, Option<String>)>);

    impl FakeFiles {
        fn with(mut self, path: &str, before: Option<Option<&str>>, now: Option<&str>) -> Self {
            self.0.insert(
                path.into(),
                (
                    before.map(|b| b.map(str::to_string)),
                    now.map(str::to_string),
                ),
            );
            self
        }
    }

    impl FileSource for FakeFiles {
        fn before(&self, path: &str) -> Option<Option<String>> {
            self.0.get(path).and_then(|(before, _)| before.clone())
        }
        fn now(&self, path: &str) -> Option<String> {
            self.0.get(path).and_then(|(_, now)| now.clone())
        }
    }

    fn start_tools(app: &mut App, count: usize) {
        for i in 0..count {
            app.apply(UiEvent::ToolStarted {
                call_id: format!("c{i}"),
                name: "read_file".into(),
                args: format!(r#"{{"path":"file_{i:02}.rs"}}"#),
            });
            app.apply(UiEvent::ToolFinished {
                call_id: format!("c{i}"),
                name: "read_file".into(),
                result: format!("contents of file {i:02}\nline two of {i:02}"),
                status: ToolStatus::Ok,
                changed_files: Vec::new(),
            });
        }
    }

    fn change(app: &mut App, path: &str, kind: harness_tools::FileChangeKind) {
        app.apply(UiEvent::ToolFinished {
            call_id: format!("w-{path}"),
            name: "edit_file".into(),
            result: "ok".into(),
            status: ToolStatus::Ok,
            changed_files: vec![harness_tools::FileChange {
                path: path.into(),
                kind,
            }],
        });
    }

    #[test]
    fn tab_moves_focus_through_every_panel_and_the_border_follows() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        drawn(&app);
        let order: Vec<Panel> = (0..4)
            .map(|_| {
                app.cycle_focus(false);
                app.focus
            })
            .collect();
        assert_eq!(
            order,
            [
                Panel::Tasks,
                Panel::Tools,
                Panel::Files,
                Panel::Conversation
            ]
        );
        app.cycle_focus(true);
        assert_eq!(app.focus, Panel::Files);

        app.focus = Panel::Tools;
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let tools = app.layout.get().tools;
        let corner = &terminal.backend().buffer()[(tools.x, tools.y)];
        assert_eq!(corner.fg, ACCENT, "the focused panel has the accent border");

        app.sidebar_visible = false;
        drawn(&app);
        app.cycle_focus(false);
        assert_eq!(
            app.focus,
            Panel::Conversation,
            "hidden panels can't take focus"
        );
    }

    #[test]
    fn enter_on_a_tool_opens_its_full_request_and_result() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        start_tools(&mut app, 3);
        app.focus = Panel::Tools;
        app.step_focused(-1);
        app.open_focused(&FakeFiles::default());
        assert_eq!(
            app.detail.as_ref().map(|d| d.kind.clone()),
            Some(DetailKind::Tool("c1".into()))
        );
        let text = drawn(&app);
        assert!(text.contains("read_file · Completed"));
        assert!(text.contains("line two of 01"), "the whole result is shown");

        app.step_detail(true, &FakeFiles::default());
        assert!(
            drawn(&app).contains("line two of 02"),
            "→ moves to the next call"
        );
    }

    #[test]
    fn changed_files_show_counts_and_open_as_a_diff() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        change(
            &mut app,
            "src/a.rs",
            harness_tools::FileChangeKind::Modified,
        );
        change(&mut app, "new.txt", harness_tools::FileChangeKind::Created);
        let files = FakeFiles::default()
            .with(
                "src/a.rs",
                Some(Some("one\ntwo\nthree")),
                Some("one\n2\nthree"),
            )
            .with("new.txt", Some(None), Some("x\ny"));
        app.refresh_file_stats(&files);
        assert_eq!(app.changed_files[0].stats, Some((1, 1)));
        assert_eq!(app.changed_files[1].stats, Some((2, 0)));
        let panel = drawn(&app);
        assert!(panel.contains("src/a.rs") && panel.contains("+1") && panel.contains("−1"));

        app.focus = Panel::Files;
        app.open_focused(&files);
        let diff = drawn(&app);
        assert!(diff.contains(" - two") && diff.contains(" + 2"));

        app.step_detail(true, &files);
        assert!(drawn(&app).contains("New file · 2 lines"));
    }

    #[test]
    fn slash_diff_lists_every_changed_file() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        change(&mut app, "a.rs", harness_tools::FileChangeKind::Modified);
        change(&mut app, "b.rs", harness_tools::FileChangeKind::Modified);
        let files = FakeFiles::default()
            .with("a.rs", Some(Some("1")), Some("2"))
            .with("b.rs", Some(Some("x")), Some("y"));
        app.open_all_changes(&files);
        let text = drawn(&app);
        assert!(text.contains("Changes this session · 2 file(s)"));
        assert!(text.contains(" + 2") && text.contains(" + y"));
    }

    #[test]
    fn the_detail_view_scrolls_within_its_content() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.apply(UiEvent::ToolStarted {
            call_id: "big".into(),
            name: "run_shell".into(),
            args: r#"{"command":"seq 200"}"#.into(),
        });
        let output = (1..=200)
            .map(|i| format!("row-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.apply(UiEvent::ToolFinished {
            call_id: "big".into(),
            name: "run_shell".into(),
            result: output,
            status: ToolStatus::Ok,
            changed_files: Vec::new(),
        });
        app.focus = Panel::Tools;
        app.open_focused(&FakeFiles::default());
        assert!(!drawn(&app).contains("row-200"));
        app.scroll_detail(isize::MAX / 2);
        let bottom = drawn(&app);
        assert!(bottom.contains("row-200") && !bottom.contains("row-001"));
        let layout = app.layout.get();
        assert_eq!(
            app.detail.as_ref().unwrap().scroll,
            layout.detail_max_scroll
        );
    }

    #[test]
    fn clicking_a_tool_selects_it_and_a_second_click_opens_it() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        start_tools(&mut app, 5);
        drawn(&app);
        let tools = app.layout.get().tools;
        // Card 1 starts on the list's third row (two rows per card).
        assert!(!app.click_at(tools.x + 3, tools.y + 1 + 2));
        assert_eq!((app.focus, app.selected_tool), (Panel::Tools, 1));
        assert!(
            app.click_at(tools.x + 3, tools.y + 1 + 3),
            "same card again"
        );
    }

    /// Text inside one panel, so other panels can't satisfy an assertion.
    fn drawn_in(app: &App, area: impl Fn(&App) -> Rect) -> String {
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let rect = area(app);
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_wheel_over_tools_moves_the_list_not_just_the_highlight() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        start_tools(&mut app, 40);
        let tools_panel = |app: &App| app.layout.get().tools;
        let at_end = drawn_in(&app, tools_panel);
        assert!(at_end.contains("file_39.rs") && !at_end.contains("file_00.rs"));
        let tools = app.layout.get().tools;
        for _ in 0..60 {
            app.scroll_at(tools.x + 2, tools.y + 2, false);
        }
        let top = drawn_in(&app, tools_panel);
        assert!(top.contains("file_00.rs") && !top.contains("file_39.rs"));
        assert_eq!(app.tools_offset.get(), 0);
    }

    #[test]
    fn tool_calls_appear_in_the_conversation_where_they_happened() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.submitted("check the site");
        app.apply(UiEvent::AssistantDelta("Checking.\n".into()));
        app.apply(UiEvent::ToolStarted {
            call_id: "a".into(),
            name: "edit_file".into(),
            args: r#"{"path":"script.js","old_string":"a","new_string":"b"}"#.into(),
        });
        app.apply(UiEvent::ToolStarted {
            call_id: "b".into(),
            name: "run_shell".into(),
            args: r#"{"command":"node --check script.js"}"#.into(),
        });
        app.apply(UiEvent::ToolFinished {
            call_id: "b".into(),
            name: "run_shell".into(),
            result: "SyntaxError: Unexpected token".into(),
            status: ToolStatus::Failed,
            changed_files: Vec::new(),
        });
        app.apply(UiEvent::AssistantDelta("Fixed it.".into()));
        let chat = drawn_in(&app, |app| app.layout.get().conversation);
        let edit = chat.find("Edit  script.js  +1 −1").expect("edit row");
        let shell = chat
            .find("Shell  node --check script.js")
            .expect("shell row");
        let reason = chat
            .find("↳ SyntaxError: Unexpected token")
            .expect("failure reason");
        let after = chat.find("Fixed it.").expect("reply after the tools");
        assert!(chat.find("Checking.").unwrap() < edit && edit < shell);
        assert!(shell < reason && reason < after);
    }

    #[test]
    fn replies_render_markdown_and_headings_are_marked() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.submitted("explain");
        app.apply(UiEvent::AssistantDelta(
            "## Key features\n- **Cart** persists\n```js\nlet x = **y**;\n```\n".into(),
        ));
        let chat = drawn_in(&app, |app| app.layout.get().conversation);
        assert!(chat.contains("▌You") && chat.contains("▌HiveMind"));
        assert!(chat.contains("Key features") && !chat.contains("## Key"));
        assert!(chat.contains("• Cart persists"));
        assert!(chat.contains("│ let x = **y**;"), "code is left verbatim");
    }

    #[test]
    fn the_message_box_title_says_what_is_running_and_how_to_stop_it() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.apply(UiEvent::TurnStarted);
        assert!(drawn(&app).contains("Thinking"));
        app.apply(UiEvent::ToolStarted {
            call_id: "a".into(),
            name: "run_shell".into(),
            args: r#"{"command":"curl -s http://localhost:8080"}"#.into(),
        });
        let text = drawn(&app);
        assert!(text.contains("Shell curl -s http://localhost:8080"));
        assert!(text.contains("Esc to stop"));
        app.apply(UiEvent::RunFinished { error: None });
        let idle = drawn(&app);
        assert!(idle.contains("Message HiveMind") && !idle.contains("Esc to stop"));
    }

    #[test]
    fn the_transcript_expands_every_tool_call_inline() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.submitted("look");
        start_tools(&mut app, 2);
        app.open_transcript();
        let text = drawn(&app);
        assert!(text.contains("Transcript · every tool call expanded"));
        assert!(text.contains("line two of 00") && text.contains("line two of 01"));
    }

    #[test]
    fn question_mark_help_lists_keys_and_commands() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.open_help();
        let text = drawn(&app);
        assert!(text.contains("Keys and commands"));
        assert!(text.contains("Ctrl+O") && text.contains("next / previous panel"));
    }

    #[test]
    fn the_header_shows_the_connection_and_known_context_use() {
        let mut app = App::new(
            "work".into(),
            "hivemind".into(),
            &[],
            Vec::new(),
            Vec::new(),
        );
        app.connection = "openrouter.ai".into();
        let window = harness_config::lookup_model("hivemind")
            .unwrap()
            .context_window;
        app.apply(UiEvent::Usage {
            model: "hivemind".into(),
            prompt_tokens: window / 4,
            completion_tokens: 10,
            turn_cost_usd: Some(0.01),
            billed: true,
            session_cost_usd: 0.01,
        });
        let text = drawn(&app);
        assert!(text.contains("openrouter.ai · model: hivemind"));
        assert!(text.contains("ctx 25%"));

        app.model = "some/unknown-model".into();
        assert!(
            !drawn(&app).contains("ctx "),
            "no guess for an unknown window"
        );
    }

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

    fn usage(prompt: u64, cost: Option<f64>, billed: bool, session: f64) -> UiEvent {
        UiEvent::Usage {
            model: "model".into(),
            prompt_tokens: prompt,
            completion_tokens: 130,
            turn_cost_usd: cost,
            billed,
            session_cost_usd: session,
        }
    }

    #[test]
    fn the_footer_shows_an_unknown_price_as_unknown_not_zero() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.apply(usage(20_844, None, true, 0.0));
        let text = drawn(&app);
        assert!(text.contains("20.8k in · 130 out · cost n/a"), "{text}");
        assert!(!text.contains("$0.0000"));

        let mut known = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        known.apply(usage(1_000, Some(0.0042), true, 0.0042));
        assert!(drawn(&known).contains("$0.0042"));

        let mut local = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        local.apply(usage(1_000, None, false, 0.0));
        assert!(drawn(&local).contains("local model"));
    }

    #[test]
    fn every_task_is_listed_and_the_panel_scrolls_under_the_wheel() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        let list = (0..30)
            .map(|i| format!("[{}] task number {i:02}", if i < 2 { "x" } else { " " }))
            .collect::<Vec<_>>()
            .join("\n");
        app.apply(UiEvent::ToolFinished {
            call_id: "t".into(),
            name: "todo_write".into(),
            result: list,
            status: ToolStatus::Ok,
            changed_files: Vec::new(),
        });
        let first = drawn(&app);
        assert!(first.contains("Tasks 2/30"));
        assert!(first.contains("task number 00") && !first.contains("task number 29"));

        let tasks = app.layout.get().tasks;
        for _ in 0..40 {
            app.scroll_at(tasks.x + 2, tasks.y + 2, true);
        }
        let scrolled = drawn(&app);
        assert!(scrolled.contains("task number 29") && !scrolled.contains("task number 00"));
    }

    #[test]
    fn a_failed_tool_card_says_why() {
        let mut app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        app.apply(UiEvent::ToolStarted {
            call_id: "c".into(),
            name: "run_shell".into(),
            args: r#"{"command":"node --check script.js"}"#.into(),
        });
        app.apply(UiEvent::ToolFinished {
            call_id: "c".into(),
            name: "run_shell".into(),
            result: "\nSyntaxError: Unexpected token (42:7)\nmore".into(),
            status: ToolStatus::Failed,
            changed_files: Vec::new(),
        });
        let text = drawn(&app);
        assert!(text.contains("node --check script.js"));
        assert!(text.contains("SyntaxError: Unexpected token"));
    }

    #[test]
    fn the_placeholder_is_not_hidden_under_the_cursor() {
        let app = App::new("work".into(), "model".into(), &[], Vec::new(), Vec::new());
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(cursor.x, cursor.y)].symbol(), " ");
        assert!(screen(&terminal).contains("Ask HiveMind"));
    }

    fn choice(
        id: &str,
        name: Option<&str>,
        window: Option<u64>,
        prices: Option<(f64, f64)>,
    ) -> ModelChoice {
        ModelChoice {
            id: id.into(),
            name: name.map(str::to_string),
            context_window: window,
            input_per_m: prices.map(|p| p.0),
            output_per_m: prices.map(|p| p.1),
        }
    }

    fn picker_app(current: &str, choices: Vec<ModelChoice>) -> App {
        let mut app = App::new("work".into(), current.into(), &[], Vec::new(), Vec::new());
        app.open_model_picker();
        app.apply(UiEvent::ModelChoices {
            current: current.into(),
            result: Ok(choices),
        });
        app
    }

    #[test]
    fn the_workspace_path_loses_the_windows_verbatim_prefix() {
        assert_eq!(
            display_workspace(r"\\?\C:\Users\me\proj"),
            r"C:\Users\me\proj"
        );
        assert_eq!(
            display_workspace(r"\\?\UNC\server\share\proj"),
            r"\\server\share\proj"
        );
        assert_eq!(display_workspace(r"C:\plain"), r"C:\plain");
        assert_eq!(display_workspace("/home/me/proj"), "/home/me/proj");
        let app = App::new(
            r"\\?\C:\Users\me\proj".into(),
            "m".into(),
            &[],
            Vec::new(),
            Vec::new(),
        );
        let text = drawn(&app);
        assert!(text.contains(r"workspace: C:\Users\me\proj"), "{text}");
        assert!(!text.contains(r"\\?\"));
    }

    #[test]
    fn the_model_picker_shows_loading_then_real_choices_with_size_and_price() {
        let mut app = App::new(
            "work".into(),
            "deepseek/deepseek-v4-flash".into(),
            &[],
            Vec::new(),
            Vec::new(),
        );
        app.open_model_picker();
        let loading = drawn(&app);
        assert!(
            loading.contains("Choose a model") && loading.contains("Loading"),
            "{loading}"
        );

        app.apply(UiEvent::ModelChoices {
            current: "deepseek/deepseek-v4-flash".into(),
            result: Ok(vec![
                choice(
                    "anthropic/claude-sonnet-5",
                    Some("Claude Sonnet 5"),
                    Some(1_000_000),
                    Some((3.0, 15.0)),
                ),
                choice(
                    "deepseek/deepseek-v4-flash",
                    Some("DeepSeek V4 Flash"),
                    Some(1_048_576),
                    Some((0.14, 0.28)),
                ),
                choice("tiny/model", None, None, None),
            ]),
        });
        let text = drawn(&app);
        assert!(
            text.contains("● deepseek/deepseek-v4-flash"),
            "the model in use is marked: {text}"
        );
        assert!(
            text.contains("1M ctx") && text.contains("$0.14 in / $0.28 out per 1M"),
            "{text}"
        );
        assert!(text.contains("anthropic/claude-sonnet-5") && text.contains("tiny/model"));
        assert!(text.contains("Filter:") && text.contains("3 of 3"));
        let order: Vec<usize> = [
            "deepseek/deepseek-v4-flash",
            "anthropic/claude-sonnet-5",
            "tiny/model",
        ]
        .iter()
        .map(|id| text.find(id).unwrap())
        .collect();
        assert!(
            order[0] < order[1] && order[1] < order[2],
            "the current model leads the list"
        );
    }

    #[test]
    fn typing_filters_the_models_and_enter_picks_the_top_match() {
        let mut app = picker_app(
            "a/one",
            vec![
                choice("a/one", None, None, None),
                choice("b/claude-sonnet", None, None, None),
                choice("c/claude-haiku", None, None, None),
            ],
        );
        for ch in "claude".chars() {
            app.picker.as_mut().unwrap().type_char(ch);
        }
        let list = drawn_in(&app, |a| a.layout.get().picker);
        assert!(list.contains("2 of 3") && !list.contains("a/one"), "{list}");
        app.picker.as_mut().unwrap().move_by(1);
        assert_eq!(
            app.apply_picker(),
            Some(PickerChoice::Model("c/claude-haiku".into()))
        );
        assert!(app.picker.is_none(), "choosing closes the list");

        let mut none = picker_app("a/one", vec![choice("a/one", None, None, None)]);
        for ch in "zzz".chars() {
            none.picker.as_mut().unwrap().type_char(ch);
        }
        assert!(drawn(&none).contains("Nothing matches"));
        assert_eq!(none.apply_picker(), None, "no match, nothing to pick");
        none.picker.as_mut().unwrap().backspace();
        none.picker.as_mut().unwrap().backspace();
        none.picker.as_mut().unwrap().backspace();
        assert!(none.apply_picker().is_some());
    }

    #[test]
    fn a_long_model_list_scrolls_and_keeps_the_highlight_in_view() {
        let many: Vec<ModelChoice> = (0..464)
            .map(|i| choice(&format!("vendor/model-{i:03}"), None, None, None))
            .collect();
        let mut app = picker_app("vendor/model-000", many);
        assert!(drawn(&app).contains("464 of 464"));
        app.picker.as_mut().unwrap().move_by(isize::MAX / 2);
        let end = drawn(&app);
        assert!(
            end.contains("vendor/model-463"),
            "End reaches the last model: {end}"
        );
        assert!(!end.contains("vendor/model-001"));
        app.picker.as_mut().unwrap().move_by(-5);
        assert!(drawn(&app).contains("vendor/model-458"));
    }

    #[test]
    fn a_provider_that_lists_nothing_still_lets_you_keep_or_type_a_model() {
        let mut app = App::new(
            "work".into(),
            "my/model".into(),
            &[],
            Vec::new(),
            Vec::new(),
        );
        app.open_model_picker();
        app.apply(UiEvent::ModelChoices {
            current: "my/model".into(),
            result: Err("This provider doesn't publish a list of its models".into()),
        });
        let text = drawn(&app);
        assert!(
            text.contains("doesn't publish a list of its models"),
            "{text}"
        );
        assert!(
            text.contains("Use /model <id>") && text.contains("● my/model"),
            "{text}"
        );
    }

    #[test]
    fn the_reasoning_picker_lists_off_and_exactly_the_levels_the_model_accepts() {
        let mut app = App::new(
            "work".into(),
            "deepseek/deepseek-v4-flash".into(),
            &[],
            Vec::new(),
            Vec::new(),
        );
        app.open_reasoning_picker();
        app.apply(UiEvent::ReasoningChoices {
            model: "deepseek/deepseek-v4-flash".into(),
            current: Some("high".into()),
            levels: vec!["xhigh".into(), "high".into(), "none".into()],
            verified: true,
        });
        let text = drawn(&app);
        assert!(
            text.contains("How much should deepseek/deepseek-v4-flash think?"),
            "{text}"
        );
        for level in ["off", "xhigh", "high", "none"] {
            assert!(text.contains(level), "{level} missing: {text}");
        }
        assert!(
            !text.contains("medium"),
            "only levels the model accepts: {text}"
        );
        assert!(text.contains("● high") && !text.contains("● off"), "{text}");
        assert!(
            !text.contains("sent as-is"),
            "verified levels carry no warning"
        );
        assert!(!text.contains("Filter:"), "a short list needs no filter");

        assert_eq!(
            app.picker.as_ref().unwrap().chosen().unwrap().value,
            "high",
            "starts on the current level"
        );
        app.picker.as_mut().unwrap().move_by(1);
        assert_eq!(
            app.apply_picker(),
            Some(PickerChoice::Reasoning(Some("none".into())))
        );
    }

    #[test]
    fn choosing_off_turns_reasoning_off() {
        let mut app = App::new("work".into(), "m".into(), &[], Vec::new(), Vec::new());
        app.open_reasoning_picker();
        app.apply(UiEvent::ReasoningChoices {
            model: "m".into(),
            current: Some("high".into()),
            levels: vec!["high".into()],
            verified: true,
        });
        app.picker.as_mut().unwrap().move_by(-1);
        assert_eq!(app.apply_picker(), Some(PickerChoice::Reasoning(None)));
    }

    #[test]
    fn unverified_levels_say_they_are_a_guess() {
        let mut app = App::new("work".into(), "llama3".into(), &[], Vec::new(), Vec::new());
        app.open_reasoning_picker();
        app.apply(UiEvent::ReasoningChoices {
            model: "llama3".into(),
            current: None,
            levels: vec!["low".into(), "medium".into(), "high".into()],
            verified: false,
        });
        let text = drawn(&app);
        assert!(
            text.contains("can't tell which levels this model accepts"),
            "{text}"
        );
        assert!(
            text.contains("sent as-is") && text.contains("● off"),
            "{text}"
        );
    }

    #[test]
    fn a_model_with_no_reasoning_gets_a_note_instead_of_an_empty_list() {
        let mut app = App::new(
            "work".into(),
            "grok-build".into(),
            &[],
            Vec::new(),
            Vec::new(),
        );
        app.open_reasoning_picker();
        app.apply(UiEvent::ReasoningChoices {
            model: "grok-build".into(),
            current: None,
            levels: Vec::new(),
            verified: true,
        });
        assert!(app.picker.is_none());
        assert!(app.conversation.iter().any(|l| {
            l.text
                .contains("grok-build has no adjustable reasoning level")
        }));
    }

    #[test]
    fn a_reply_that_arrives_after_the_picker_was_closed_is_ignored() {
        let mut app = App::new("work".into(), "m".into(), &[], Vec::new(), Vec::new());
        app.open_model_picker();
        app.close_picker();
        app.apply(UiEvent::ModelChoices {
            current: "m".into(),
            result: Ok(vec![choice("x/y", None, None, None)]),
        });
        assert!(app.picker.is_none());
    }

    #[test]
    fn clicking_a_row_chooses_it_and_the_wheel_moves_the_highlight() {
        let mut app = picker_app(
            "a/one",
            vec![
                choice("a/one", None, None, None),
                choice("b/two", None, None, None),
                choice("c/three", None, None, None),
            ],
        );
        drawn(&app);
        let list = app.layout.get().picker_list;
        app.scroll_at(list.x + 2, list.y, true);
        assert_eq!(
            app.picker.as_ref().unwrap().selected,
            2,
            "the wheel moves the highlight, clamped"
        );
        assert!(app.picker.is_some(), "scrolling alone does not choose");
        assert_eq!(
            app.click_picker(list.x + 4, list.y + 1),
            Some(PickerChoice::Model("b/two".into()))
        );
        assert!(app.picker.is_none());

        let mut outside = picker_app("a/one", vec![choice("a/one", None, None, None)]);
        drawn(&outside);
        assert_eq!(
            outside.click_picker(0, 0),
            None,
            "a click outside the rows chooses nothing"
        );
        assert!(outside.picker.is_some());
    }

    #[test]
    fn the_command_hints_step_aside_while_a_picker_is_open() {
        let mut app = picker_app("a/one", vec![choice("a/one", None, None, None)]);
        app.composer = "/mo".into();
        assert!(!drawn(&app).contains("Tab completes"));
        app.close_picker();
        assert!(drawn(&app).contains("Tab completes"));
    }

    #[test]
    fn the_header_shows_the_reasoning_level_once_one_is_set() {
        let mut app = App::new("work".into(), "m".into(), &[], Vec::new(), Vec::new());
        assert!(!drawn(&app).contains("reasoning "));
        app.apply(UiEvent::ReasoningChanged(Some("high".into())));
        assert!(drawn(&app).contains("reasoning high"));
        app.apply(UiEvent::ReasoningChanged(None));
        assert!(!drawn(&app).contains("reasoning high"));
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
