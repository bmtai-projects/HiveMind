//! Just enough Markdown for model replies: headings, bold, inline code,
//! lists, quotes, rules and fenced code. Anything else stays plain text.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::render::{ACCENT, MUTED};

const TEXT: Color = Color::White;
const CODE: Color = Color::Rgb(255, 203, 139);

/// Renders one reply line. `in_code` carries fenced-block state across lines.
pub(crate) fn render_line(text: &str, in_code: &mut bool) -> Line<'static> {
    let trimmed = text.trim_start();
    if trimmed.starts_with("```") {
        *in_code = !*in_code;
        let label = trimmed.trim_start_matches('`').trim();
        return Line::styled(
            if *in_code && !label.is_empty() {
                format!("╭─ {label}")
            } else if *in_code {
                "╭─".to_string()
            } else {
                "╰─".to_string()
            },
            Style::default().fg(MUTED),
        );
    }
    if *in_code {
        return Line::from(vec![
            Span::styled("│ ", Style::default().fg(MUTED)),
            Span::styled(text.to_string(), Style::default().fg(CODE)),
        ]);
    }
    if let Some(heading) = heading_text(trimmed) {
        return Line::from(inline(
            heading,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    if matches!(trimmed, "---" | "***" | "___") {
        return Line::styled("─".repeat(40), Style::default().fg(MUTED));
    }
    if let Some(quote) = trimmed
        .strip_prefix("> ")
        .or(trimmed.strip_prefix('>').filter(|q| q.is_empty()))
    {
        let mut spans = vec![Span::styled("│ ", Style::default().fg(MUTED))];
        spans.extend(inline(
            quote,
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ));
        return Line::from(spans);
    }
    let indent = &text[..text.len() - trimmed.len()];
    if let Some(item) = ["- ", "* ", "+ "]
        .iter()
        .find_map(|p| trimmed.strip_prefix(p))
    {
        let mut spans = vec![Span::styled(
            format!("{indent}• "),
            Style::default().fg(ACCENT),
        )];
        spans.extend(inline(item, Style::default().fg(TEXT)));
        return Line::from(spans);
    }
    Line::from(inline(text, Style::default().fg(TEXT)))
}

fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    (1..=6)
        .contains(&hashes)
        .then(|| line[hashes..].strip_prefix(' '))
        .flatten()
}

/// `**bold**` and `` `code` `` inside a line; unmatched markers stay literal.
fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    let mut plain = String::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
            && end > 0
        {
            flush(&mut spans, &mut plain, base);
            spans.push(Span::styled(
                after[..end].to_string(),
                base.add_modifier(Modifier::BOLD),
            ));
            rest = &after[end + 2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
            && end > 0
        {
            flush(&mut spans, &mut plain, base);
            spans.push(Span::styled(
                after[..end].to_string(),
                Style::default().fg(CODE),
            ));
            rest = &after[end + 1..];
            continue;
        }
        let ch = rest.chars().next().expect("rest is not empty");
        plain.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush(&mut spans, &mut plain, base);
    spans
}

fn flush(spans: &mut Vec<Span<'static>>, plain: &mut String, style: Style) {
    if !plain.is_empty() {
        spans.push(Span::styled(std::mem::take(plain), style));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn render(text: &str) -> Line<'static> {
        render_line(text, &mut false)
    }

    #[test]
    fn bold_and_inline_code_lose_their_markers() {
        let line = render("Use **Jost** for `body` text");
        assert_eq!(plain(&line), "Use Jost for body text");
        let bold = line.spans.iter().find(|s| s.content == "Jost").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = line.spans.iter().find(|s| s.content == "body").unwrap();
        assert_eq!(code.style.fg, Some(CODE));
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        assert_eq!(
            plain(&render("2 ** 3 and a lone ` tick")),
            "2 ** 3 and a lone ` tick"
        );
        assert_eq!(plain(&render("****")), "****");
    }

    #[test]
    fn headings_bullets_quotes_and_rules() {
        assert_eq!(plain(&render("## Key features")), "Key features");
        assert_eq!(
            plain(&render("#hashtag")),
            "#hashtag",
            "needs a space to be a heading"
        );
        assert_eq!(plain(&render("- **Files:** three")), "• Files: three");
        assert_eq!(plain(&render("  * nested")), "  • nested");
        assert_eq!(plain(&render("> careful")), "│ careful");
        assert_eq!(plain(&render("---")), "─".repeat(40));
    }

    #[test]
    fn fenced_code_is_kept_verbatim_until_the_fence_closes() {
        let mut in_code = false;
        assert_eq!(plain(&render_line("```rust", &mut in_code)), "╭─ rust");
        assert!(in_code);
        assert_eq!(
            plain(&render_line("let x = **y**;", &mut in_code)),
            "│ let x = **y**;"
        );
        assert_eq!(plain(&render_line("```", &mut in_code)), "╰─");
        assert!(!in_code);
    }

    #[test]
    fn unicode_text_is_not_split_mid_character() {
        assert_eq!(plain(&render("नमस्ते **世界**")), "नमस्ते 世界");
    }
}
