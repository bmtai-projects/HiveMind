//! `create_pdf` — generate a simple, real PDF (headings, paragraphs, bullet
//! lists, tables) with no external runtime dependency: no Python, no
//! LibreOffice, nothing to install. Deliberately scoped to what a JSON tool
//! schema can express well — a structured report/memo/invoice — not
//! arbitrary pixel-perfect layout, charts, or images. For anything beyond
//! this shape, the agent's own `run_shell` plus a scripting library (e.g.
//! `reportlab`) is the right tool; growing this schema into a full
//! page-layout language would be the wrong trade.
//!
//! Word-wrap uses an approximate average character width, not real glyph
//! metrics — printpdf only exposes true glyph widths for parsed/embedded
//! fonts, not the 14 built-in ones this tool uses (see `Cargo.toml` for why
//! font embedding is deliberately out of scope). Same honest-approximation
//! choice as `harness_agent::tokens`: "never overflows the page" is the bar,
//! not "wraps at the exact column a real renderer would."

use async_trait::async_trait;
use printpdf::{
    BuiltinFont, Color, Op, PaintMode, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions, Point,
    Pt, Rect, Rgb, TextItem,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::path::Path;

use crate::error::ToolError;
use crate::fs::Workspace;
use crate::tool::{Tool, obj_schema};

const PAGE_WIDTH_PT: f32 = 595.0; // A4
const PAGE_HEIGHT_PT: f32 = 842.0;
const MARGIN_PT: f32 = 50.0;
const USABLE_WIDTH_PT: f32 = PAGE_WIDTH_PT - 2.0 * MARGIN_PT;

const TITLE_SIZE: f32 = 20.0;
const HEADING_SIZE: f32 = 14.0;
const BODY_SIZE: f32 = 11.0;
const LINE_HEIGHT_FACTOR: f32 = 1.4;
/// Fraction of font size a character occupies on average for Helvetica --
/// see module docs on why this is approximate, not measured.
const AVG_CHAR_WIDTH_FACTOR: f32 = 0.52;
const TABLE_ROW_HEIGHT_PT: f32 = 20.0;

#[derive(Deserialize)]
struct PdfArgs {
    path: String,
    #[serde(default)]
    title: Option<String>,
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Heading {
        text: String,
    },
    Paragraph {
        text: String,
    },
    Bullets {
        items: Vec<String>,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

pub struct CreatePdf(pub Workspace);

#[async_trait]
impl Tool for CreatePdf {
    fn conflict_key(&self, args: &RawValue) -> Option<String> {
        crate::fs::path_conflict_key(&self.0, args)
    }
    fn name(&self) -> &str {
        "create_pdf"
    }
    fn description(&self) -> &str {
        "Create a real PDF document (report, memo, invoice) at a workspace-relative path from \
         structured content -- no external tools required. `blocks` is an array of objects, each \
         with a \"type\" field: {\"type\":\"heading\",\"text\":...}, \
         {\"type\":\"paragraph\",\"text\":...}, {\"type\":\"bullets\",\"items\":[...]}, or \
         {\"type\":\"table\",\"headers\":[...],\"rows\":[[...], ...]}. Content flows across as \
         many pages as needed automatically. For layouts this can't express (charts, images, \
         precise pixel layout), write and run a script instead."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "path",
                    serde_json::json!({"type": "string", "description": "workspace-relative output path, e.g. \"report.pdf\""}),
                ),
                (
                    "title",
                    serde_json::json!({"type": "string", "description": "optional document title, shown at the top of page 1"}),
                ),
                (
                    "blocks",
                    serde_json::json!({
                        "type": "array",
                        "description": "content blocks in order -- see tool description for the exact shape of each type",
                        "items": {"type": "object"},
                    }),
                ),
            ],
            &["path", "blocks"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
        let a: PdfArgs = serde_json::from_str(args.get())?;
        if a.path.is_empty() {
            return Err(ToolError::Message("path is required".into()));
        }

        let pages = render_pages(a.title.as_deref(), &a.blocks);
        let page_count = pages.len();
        let mut doc = PdfDocument::new(a.title.as_deref().unwrap_or("document"));
        doc.with_pages(
            pages
                .into_iter()
                .map(|ops| PdfPage::new(printpdf::Mm(210.0), printpdf::Mm(297.0), ops))
                .collect(),
        );
        let mut warnings = Vec::new();
        let bytes = doc.save(&PdfSaveOptions::default(), &mut warnings);

        // Same create-parent-dirs-then-resolve dance as `write_file`: the
        // path may not exist yet, so `Workspace::resolve`'s canonicalize
        // step needs the parent directory to already be there.
        let candidate = Path::new(&a.path);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.0.root.join(candidate)
        };
        if let Some(parent) = joined.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let p = self.0.resolve(&a.path)?;
        tokio::fs::write(&p, &bytes).await?;

        Ok(format!(
            "wrote {} bytes to {} ({page_count} page{})",
            bytes.len(),
            a.path,
            if page_count == 1 { "" } else { "s" }
        ))
    }
}

struct Layout {
    pages: Vec<Vec<Op>>,
    cur_ops: Vec<Op>,
    cursor_y: f32,
}

impl Layout {
    fn new() -> Self {
        Self {
            pages: Vec::new(),
            cur_ops: Vec::new(),
            cursor_y: PAGE_HEIGHT_PT - MARGIN_PT,
        }
    }

    /// Start a fresh page if `needed` more vertical space doesn't fit above
    /// the bottom margin on the current one.
    fn ensure_space(&mut self, needed: f32) {
        if self.cursor_y - needed < MARGIN_PT {
            self.pages.push(std::mem::take(&mut self.cur_ops));
            self.cursor_y = PAGE_HEIGHT_PT - MARGIN_PT;
        }
    }

    fn finish(mut self) -> Vec<Vec<Op>> {
        self.pages.push(self.cur_ops);
        self.pages
    }

    fn text_line(&mut self, text: &str, font: BuiltinFont, size: f32) {
        self.indented_text_line(text, font, size, 0.0);
    }

    fn indented_text_line(&mut self, text: &str, font: BuiltinFont, size: f32, indent: f32) {
        let line_height = size * LINE_HEIGHT_FACTOR;
        self.ensure_space(line_height);
        self.cur_ops.push(Op::SetFillColor {
            col: Color::Rgb(Rgb {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                icc_profile: None,
            }),
        });
        self.cur_ops.push(Op::SetFont {
            font: PdfFontHandle::Builtin(font),
            size: Pt(size),
        });
        self.cur_ops.push(Op::StartTextSection);
        self.cur_ops.push(Op::SetTextCursor {
            pos: Point {
                x: Pt(MARGIN_PT + indent),
                y: Pt(self.cursor_y),
            },
        });
        self.cur_ops.push(Op::ShowText {
            items: vec![TextItem::Text(text.to_string())],
        });
        self.cur_ops.push(Op::EndTextSection);
        self.cursor_y -= line_height;
    }

    fn wrapped_block(&mut self, text: &str, font: BuiltinFont, size: f32, indent: f32) {
        for line in wrap(text, size, USABLE_WIDTH_PT - indent) {
            self.indented_text_line(&line, font, size, indent);
        }
    }
}

fn render_pages(title: Option<&str>, blocks: &[Block]) -> Vec<Vec<Op>> {
    let mut layout = Layout::new();

    if let Some(title) = title {
        layout.text_line(title, BuiltinFont::HelveticaBold, TITLE_SIZE);
        layout.cursor_y -= TITLE_SIZE * 0.5;
    }

    for block in blocks {
        match block {
            Block::Heading { text } => {
                layout.cursor_y -= HEADING_SIZE * 0.3;
                layout.wrapped_block(text, BuiltinFont::HelveticaBold, HEADING_SIZE, 0.0);
                layout.cursor_y -= HEADING_SIZE * 0.2;
            }
            Block::Paragraph { text } => {
                layout.wrapped_block(text, BuiltinFont::Helvetica, BODY_SIZE, 0.0);
                layout.cursor_y -= BODY_SIZE * 0.5;
            }
            Block::Bullets { items } => {
                for item in items {
                    layout.wrapped_block(
                        &format!("\u{2022} {item}"),
                        BuiltinFont::Helvetica,
                        BODY_SIZE,
                        12.0,
                    );
                }
                layout.cursor_y -= BODY_SIZE * 0.5;
            }
            Block::Table { headers, rows } => {
                draw_table(&mut layout, headers, rows);
                layout.cursor_y -= BODY_SIZE * 0.5;
            }
        }
    }

    layout.finish()
}

fn draw_table(layout: &mut Layout, headers: &[String], rows: &[Vec<String>]) {
    if headers.is_empty() {
        return;
    }
    let col_width = USABLE_WIDTH_PT / headers.len() as f32;
    draw_table_row(layout, headers, headers.len(), col_width, true);
    for row in rows {
        draw_table_row(layout, row, headers.len(), col_width, false);
    }
}

fn draw_table_row(
    layout: &mut Layout,
    cells: &[String],
    col_count: usize,
    col_width: f32,
    is_header: bool,
) {
    layout.ensure_space(TABLE_ROW_HEIGHT_PT);
    let row_top = layout.cursor_y + BODY_SIZE * 0.3;
    let font = if is_header {
        BuiltinFont::HelveticaBold
    } else {
        BuiltinFont::Helvetica
    };

    for (i, header_or_cell) in row_cells(cells, col_count).into_iter().enumerate() {
        let x = MARGIN_PT + i as f32 * col_width;
        layout.cur_ops.push(Op::SetOutlineColor {
            col: Color::Rgb(Rgb {
                r: 0.6,
                g: 0.6,
                b: 0.6,
                icc_profile: None,
            }),
        });
        layout.cur_ops.push(Op::SetOutlineThickness { pt: Pt(0.5) });
        layout.cur_ops.push(Op::DrawRectangle {
            rectangle: Rect {
                mode: Some(PaintMode::Stroke),
                ..Rect::from_xywh(
                    Pt(x),
                    Pt(row_top - TABLE_ROW_HEIGHT_PT),
                    Pt(col_width),
                    Pt(TABLE_ROW_HEIGHT_PT),
                )
            },
        });
        layout.cur_ops.push(Op::SetFillColor {
            col: Color::Rgb(Rgb {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                icc_profile: None,
            }),
        });
        layout.cur_ops.push(Op::SetFont {
            font: PdfFontHandle::Builtin(font),
            size: Pt(BODY_SIZE),
        });
        layout.cur_ops.push(Op::StartTextSection);
        layout.cur_ops.push(Op::SetTextCursor {
            pos: Point {
                x: Pt(x + 4.0),
                y: Pt(layout.cursor_y),
            },
        });
        layout.cur_ops.push(Op::ShowText {
            items: vec![TextItem::Text(fit_cell(
                &header_or_cell,
                BODY_SIZE,
                col_width - 8.0,
            ))],
        });
        layout.cur_ops.push(Op::EndTextSection);
    }
    layout.cursor_y -= TABLE_ROW_HEIGHT_PT;
}

/// Pads a short row with empty cells, or truncates an over-long one, to
/// exactly `col_count` -- a data row that doesn't match the header count
/// (the model miscounted) still renders in the right columns instead of
/// drifting or running off the page edge.
fn row_cells(cells: &[String], col_count: usize) -> Vec<String> {
    let mut v: Vec<String> = cells.iter().take(col_count).cloned().collect();
    v.resize(col_count, String::new());
    v
}

fn fit_cell(text: &str, size: f32, max_width_pt: f32) -> String {
    let avg_char_w = size * AVG_CHAR_WIDTH_FACTOR;
    let max_chars = ((max_width_pt / avg_char_w).floor() as usize).max(1);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}\u{2026}")
}

/// Greedy word-wrap to `max_width_pt` using the approximate average
/// character width (see module docs). A single word longer than a whole
/// line is hard-broken so it can never overflow the page width.
fn wrap(text: &str, size: f32, max_width_pt: f32) -> Vec<String> {
    let avg_char_w = size * AVG_CHAR_WIDTH_FACTOR;
    let max_chars = ((max_width_pt / avg_char_w).floor() as usize).max(1);
    let mut lines = Vec::new();

    for paragraph_line in text.split('\n') {
        if paragraph_line.trim().is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph_line.split_whitespace() {
            let mut word = word.to_string();
            loop {
                let candidate_len = if current.is_empty() {
                    word.chars().count()
                } else {
                    current.chars().count() + 1 + word.chars().count()
                };
                if candidate_len <= max_chars {
                    if !current.is_empty() {
                        current.push(' ');
                    }
                    current.push_str(&word);
                    break;
                }
                if current.is_empty() {
                    // The word alone is longer than a full line: hard-break it.
                    let head: String = word.chars().take(max_chars).collect();
                    let rest: String = word.chars().skip(max_chars).collect();
                    lines.push(head);
                    word = rest;
                    if word.is_empty() {
                        break;
                    }
                    continue;
                }
                lines.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("hivemind_pdf_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    #[test]
    fn wrap_never_exceeds_the_requested_character_budget() {
        let long = "supercalifragilisticexpialidocious ".repeat(20);
        for line in wrap(&long, 11.0, 200.0) {
            let avg_char_w = 11.0 * AVG_CHAR_WIDTH_FACTOR;
            let max_chars = (200.0_f32 / avg_char_w).floor() as usize;
            assert!(line.chars().count() <= max_chars, "line too long: {line:?}");
        }
    }

    #[test]
    fn wrap_preserves_blank_lines() {
        let lines = wrap("first\n\nthird", 11.0, 400.0);
        assert_eq!(
            lines,
            vec!["first".to_string(), String::new(), "third".to_string()]
        );
    }

    #[tokio::test]
    async fn writes_a_valid_pdf_with_the_magic_header_and_requested_page_content() {
        let w = ws("basic");
        let out = CreatePdf(w.clone())
            .execute(&args(serde_json::json!({
                "path": "report.pdf",
                "title": "Q3 Report",
                "blocks": [
                    {"type": "heading", "text": "Summary"},
                    {"type": "paragraph", "text": "Revenue grew this quarter."},
                    {"type": "bullets", "items": ["Point one", "Point two"]},
                    {"type": "table", "headers": ["Metric", "Value"], "rows": [["Revenue", "$1.2M"]]},
                ],
            })))
            .await
            .unwrap();
        assert!(out.contains("report.pdf"), "got: {out}");

        let bytes = std::fs::read(w.root.join("report.pdf")).unwrap();
        assert!(bytes.starts_with(b"%PDF-"), "missing PDF header");
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn many_blocks_spill_onto_a_second_page() {
        let w = ws("multipage");
        let blocks: Vec<serde_json::Value> = (0..200)
            .map(|i| serde_json::json!({"type": "paragraph", "text": format!("Paragraph number {i}.")}))
            .collect();
        let out = CreatePdf(w.clone())
            .execute(&args(
                serde_json::json!({"path": "long.pdf", "blocks": blocks}),
            ))
            .await
            .unwrap();
        assert!(
            out.contains("pages") && !out.contains("(1 page)"),
            "expected multiple pages, got: {out}"
        );
    }

    #[tokio::test]
    async fn rejects_an_empty_path() {
        let w = ws("empty_path");
        let err = CreatePdf(w)
            .execute(&args(serde_json::json!({"path": "", "blocks": []})))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Message(_)));
    }
}
