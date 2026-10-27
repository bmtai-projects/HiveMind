//! The scrollable detail view: a tool call's full request and result, or a
//! changed file's diff against how the session found it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph, ScrollbarState, Wrap};

use crate::diff::{self, Op};

use super::app::{App, DetailKind, ToolCard, stats_label};
use super::render::{ACCENT, GREEN, MUTED, RED, boxed, render_scrollbar, tool_style};

/// Past this, a view says how much was left out instead of drawing it all.
const MAX_LINES: usize = 5_000;

/// A changed file: its path, its session baseline, and its current contents.
pub(crate) type FileVersions = (String, Option<Option<String>>, Option<String>);

pub(crate) fn render_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(detail) = &app.detail else {
        return;
    };
    let (title, lines, border) = match &detail.kind {
        DetailKind::Tool(id) => match app.tools.iter().find(|card| &card.call_id == id) {
            Some(card) => (
                format!(" {} · {} ", card.name, card.state.label()),
                tool_lines(card),
                tool_style(card.state),
            ),
            None => (
                " Tool ".to_string(),
                vec![note("This call is no longer in the list.")],
                Style::default().fg(ACCENT),
            ),
        },
        // Rebuilt every frame so a running request keeps filling in.
        DetailKind::Transcript => (
            detail.title.clone(),
            super::render::conversation_lines(app, true),
            Style::default().fg(ACCENT),
        ),
        DetailKind::File(_) | DetailKind::Changes | DetailKind::Help => (
            detail.title.clone(),
            detail.lines.clone(),
            Style::default().fg(ACCENT),
        ),
    };
    let hint = match detail.kind {
        DetailKind::Tool(_) | DetailKind::File(_) => {
            " ↑↓ PgUp PgDn scroll · ←→ previous/next · Esc close "
        }
        _ => " ↑↓ PgUp PgDn scroll · Esc close ",
    };
    let block = boxed(title).border_style(border).title_bottom(hint);
    let inner = block.inner(area);
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width);
    let page = inner.height as usize;
    let max_scroll = total.saturating_sub(page);
    let top = detail.scroll.min(max_scroll);

    let mut layout = app.layout.get();
    layout.detail_page = page;
    layout.detail_max_scroll = max_scroll;
    app.layout.set(layout);

    frame.render_widget(Clear, area);
    frame.render_widget(
        paragraph
            .block(block)
            .scroll((top.min(u16::MAX as usize) as u16, 0)),
        area,
    );
    if max_scroll > 0 {
        let mut state = ScrollbarState::new(max_scroll + 1)
            .viewport_content_length(page)
            .position(top);
        render_scrollbar(frame, area, &mut state);
    }
}

/// Everything about one call: what was asked, and everything it returned.
pub(crate) fn tool_lines(card: &ToolCard) -> Vec<Line<'static>> {
    let mut out = vec![
        field("Status", card.state.label()),
        field("Call", &card.call_id),
        Line::raw(""),
        heading("Request"),
    ];
    let args = serde_json::from_str::<serde_json::Value>(&card.args).ok();
    let text = |key: &str| args.as_ref()?.get(key)?.as_str();
    match card.name.as_str() {
        "edit_file" if text("old_string").is_some() && text("new_string").is_some() => {
            out.push(field("Path", text("path").unwrap_or("?")));
            out.extend(diff_lines(&diff::diff(
                text("old_string").unwrap_or_default(),
                text("new_string").unwrap_or_default(),
            )));
        }
        "write_file" if text("content").is_some() => {
            let content = text("content").unwrap_or_default();
            out.push(field("Path", text("path").unwrap_or("?")));
            out.push(field("Lines", &content.lines().count().to_string()));
            out.extend(capped(
                content.lines().map(|l| styled(format!(" + {l}"), GREEN)),
            ));
        }
        "run_shell" if text("command").is_some() => {
            out.extend(
                text("command")
                    .unwrap_or_default()
                    .lines()
                    .map(|l| Line::styled(format!("$ {l}"), Style::default().fg(Color::White))),
            );
        }
        "todo_write" => out.extend(todo_lines(args.as_ref())),
        _ => match &args {
            Some(value) => {
                let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
                out.extend(capped(pretty.lines().map(|l| styled(l.to_string(), MUTED))));
            }
            None if card.args.trim().is_empty() => out.push(note("No arguments recorded.")),
            None => out.push(styled(card.args.clone(), MUTED)),
        },
    }
    out.push(Line::raw(""));
    out.push(heading("Result"));
    if card.result.is_empty() {
        out.push(note("No result yet."));
    } else {
        out.extend(capped(
            card.result.lines().map(|l| Line::raw(l.to_string())),
        ));
    }
    out
}

/// One file's change since the session first touched it.
pub(crate) fn file_lines(
    before: Option<Option<String>>,
    now: Option<String>,
) -> Vec<Line<'static>> {
    match (before, now) {
        (None, now) => {
            let mut out = vec![note(
                "No before-edit snapshot for this file, so there is no diff. It was \
                 changed outside edit_file/write_file, for example by a shell command.",
            )];
            if let Some(now) = now {
                out.push(Line::raw(""));
                out.push(heading("Current contents"));
                out.extend(capped(now.lines().map(|l| Line::raw(l.to_string()))));
            }
            out
        }
        (Some(None), Some(now)) => {
            let mut out = vec![heading(&format!(
                "New file · {} lines",
                now.lines().count()
            ))];
            out.extend(capped(
                now.lines().map(|l| styled(format!(" + {l}"), GREEN)),
            ));
            out
        }
        (Some(None), None) => vec![note("Created and then removed again; nothing to show.")],
        (Some(Some(before)), None) => {
            let mut out = vec![heading("Deleted")];
            out.extend(capped(
                before.lines().map(|l| styled(format!(" - {l}"), RED)),
            ));
            out
        }
        (Some(Some(before)), Some(now)) => {
            let lines = diff::diff(&before, &now);
            if diff::stats(&lines).is_empty() {
                vec![note(
                    "No changes: the file is back to how the session found it.",
                )]
            } else {
                diff_lines(&lines)
            }
        }
    }
}

/// Every changed file, each under its own header.
pub(crate) fn changes_lines(files: &[FileVersions]) -> Vec<Line<'static>> {
    if files.is_empty() {
        return vec![note("Nothing has changed this session.")];
    }
    let mut out = Vec::new();
    for (path, before, now) in files {
        let stats = match (before, now) {
            (Some(Some(b)), Some(n)) => {
                let s = diff::stats(&diff::diff(b, n));
                Some((s.added, s.removed))
            }
            (Some(None), Some(n)) => Some((n.lines().count(), 0)),
            (Some(Some(b)), None) => Some((0, b.lines().count())),
            _ => None,
        };
        out.push(Line::from(vec![
            Span::styled(
                path.clone(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(stats_label(stats), Style::default().fg(MUTED)),
        ]));
        out.extend(file_lines(before.clone(), now.clone()));
        out.push(Line::raw(""));
    }
    out
}

/// Every key and command, grouped by what you're trying to do.
pub(crate) fn help_lines() -> Vec<Line<'static>> {
    const KEYS: &[(&str, &[(&str, &str)])] = &[
        (
            "Moving around",
            &[
                ("Tab / Shift+Tab", "next / previous panel"),
                (
                    "↑ ↓  PgUp PgDn",
                    "scroll the focused panel, or move its selection",
                ),
                ("Home / End", "top / latest"),
                ("Mouse wheel", "scrolls the panel under the pointer"),
                (
                    "Click",
                    "focus a panel; click a selected row again to open it",
                ),
            ],
        ),
        (
            "Looking closer",
            &[
                ("Enter", "open the selected tool call or changed file"),
                ("← →", "previous / next item in an open view"),
                (
                    "Ctrl+O",
                    "transcript: everything so far, tool calls expanded",
                ),
                ("Esc", "close a view, leave a panel, or stop a run"),
            ],
        ),
        (
            "Writing and running",
            &[
                ("Enter", "send  ·  Shift+Enter for a new line"),
                ("Ctrl+C", "stop the run, or clear the message box"),
                ("Ctrl+L", "clear the screen  ·  Ctrl+S hide the side panel"),
                ("Ctrl+D", "quit"),
            ],
        ),
        (
            "Copying",
            &[
                ("Shift+drag", "select text (Option+drag in iTerm2)"),
                ("/copy", "copy the last reply to the clipboard"),
            ],
        ),
    ];
    let mut out = Vec::new();
    for (group, keys) in KEYS {
        out.push(heading(group));
        for (key, what) in *keys {
            out.push(Line::from(vec![
                Span::styled(format!("  {key:<18}"), Style::default().fg(ACCENT)),
                Span::raw(what.to_string()),
            ]));
        }
        out.push(Line::raw(""));
    }
    out.push(heading("Commands"));
    for (name, what) in super::app::SLASH_COMMANDS {
        out.push(Line::from(vec![
            Span::styled(format!("  {name:<18}"), Style::default().fg(ACCENT)),
            Span::raw(what.to_string()),
        ]));
    }
    out
}

/// Changes with three lines of context, long unchanged runs folded to `⋯`.
fn diff_lines(lines: &[diff::Line<'_>]) -> Vec<Line<'static>> {
    let keep = diff::lines_worth_showing(lines);
    let mut out = Vec::new();
    let mut folded = false;
    for (line, keep) in lines.iter().zip(keep) {
        if !keep {
            if !folded {
                out.push(styled("   ⋯".into(), MUTED));
                folded = true;
            }
            continue;
        }
        folded = false;
        out.push(match line.op {
            Op::Add => styled(format!(" + {}", line.text), GREEN),
            Op::Remove => styled(format!(" - {}", line.text), RED),
            Op::Same => styled(format!("   {}", line.text), MUTED),
        });
    }
    capped(out.into_iter())
}

fn todo_lines(args: Option<&serde_json::Value>) -> Vec<Line<'static>> {
    let Some(todos) = args.and_then(|a| a.get("todos")).and_then(|t| t.as_array()) else {
        return vec![note("No task list in the request.")];
    };
    todos
        .iter()
        .map(|todo| {
            let text = todo.get("content").and_then(|c| c.as_str()).unwrap_or("?");
            let (mark, color) = match todo.get("status").and_then(|s| s.as_str()) {
                Some("completed") => ("✓", GREEN),
                Some("in_progress") => ("›", ACCENT),
                _ => ("○", MUTED),
            };
            Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(color)),
                Span::raw(text.to_string()),
            ])
        })
        .collect()
}

fn capped(lines: impl Iterator<Item = Line<'static>>) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut extra = 0usize;
    for line in lines {
        if out.len() < MAX_LINES {
            out.push(line);
        } else {
            extra += 1;
        }
    }
    if extra > 0 {
        out.push(note(&format!("… {extra} more lines not shown")));
    }
    out
}

fn field(name: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{name:<8}"), Style::default().fg(MUTED)),
        Span::raw(value.to_string()),
    ])
}

fn heading(text: &str) -> Line<'static> {
    Line::styled(
        text.to_string(),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )
}

fn note(text: &str) -> Line<'static> {
    Line::styled(
        text.to_string(),
        Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
    )
}

fn styled(text: String, color: Color) -> Line<'static> {
    Line::styled(text, Style::default().fg(color))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::ToolState;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn card(name: &str, args: &str, result: &str) -> ToolCard {
        ToolCard {
            call_id: "call-1".into(),
            name: name.into(),
            args: args.into(),
            summary: String::new(),
            result: result.into(),
            state: ToolState::Completed,
        }
    }

    #[test]
    fn an_edit_shows_its_diff_and_its_full_result() {
        let edit = card(
            "edit_file",
            r#"{"path":"a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#,
            "edited a.rs\nsecond line of output",
        );
        let shown = text(&tool_lines(&edit));
        assert!(shown.contains(" - let x = 1;") && shown.contains(" + let x = 2;"));
        assert!(
            shown.contains("second line of output"),
            "result is not truncated"
        );
    }

    #[test]
    fn unfamiliar_tools_show_pretty_arguments() {
        let shown = text(&tool_lines(&card("mystery", r#"{"alpha":1}"#, "")));
        assert!(shown.contains("\"alpha\": 1"));
        assert!(shown.contains("No result yet."));
    }

    #[test]
    fn a_file_diff_covers_new_modified_reverted_and_unknown() {
        let new = text(&file_lines(Some(None), Some("a\nb".into())));
        assert!(new.contains("New file · 2 lines") && new.contains(" + b"));

        let modified = text(&file_lines(Some(Some("a\nb".into())), Some("a\nc".into())));
        assert!(modified.contains(" - b") && modified.contains(" + c"));

        let same = text(&file_lines(Some(Some("a".into())), Some("a".into())));
        assert!(same.contains("back to how the session found it"));

        let unknown = text(&file_lines(None, Some("x".into())));
        assert!(unknown.contains("No before-edit snapshot"));
    }

    #[test]
    fn long_output_is_capped_with_a_count() {
        let result = (0..MAX_LINES + 7)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let shown = text(&tool_lines(&card(
            "run_shell",
            r#"{"command":"seq"}"#,
            &result,
        )));
        assert!(shown.contains("… 7 more lines not shown"));
    }
}
