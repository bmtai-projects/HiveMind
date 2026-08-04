//! `create_spreadsheet` — generate a real `.xlsx` workbook (one or more
//! sheets, headers, typed cells, formulas) via `rust_xlsxwriter`. No
//! external runtime required, and no LLM-side layout guesswork needed: a
//! spreadsheet's shape (rows/columns/sheets) maps directly onto JSON, unlike
//! a PDF or slide deck, so a bespoke schema is actually the right fit here
//! rather than a compromise -- see `crate::pdf` for where that stops being
//! true.

use async_trait::async_trait;
use rust_xlsxwriter::{Format, Workbook, XlsxError};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::path::Path;

use crate::error::ToolError;
use crate::fs::Workspace;
use crate::tool::{Tool, ToolResult, obj_schema};

#[derive(Deserialize)]
struct XlsxArgs {
    path: String,
    sheets: Vec<Sheet>,
}

#[derive(Deserialize)]
struct Sheet {
    name: String,
    #[serde(default)]
    headers: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<Cell>>,
}

/// A cell is either a JSON number or a JSON string -- and a string that
/// starts with `=` is written as a live Excel formula, not literal text
/// (Excel's own convention, so nothing new for the model to learn).
#[derive(Deserialize)]
#[serde(untagged)]
enum Cell {
    Number(f64),
    Text(String),
}

pub struct CreateSpreadsheet(pub Workspace);

#[async_trait]
impl Tool for CreateSpreadsheet {
    fn conflict_key(&self, args: &RawValue) -> Option<String> {
        crate::fs::path_conflict_key(&self.0, args)
    }
    fn name(&self) -> &str {
        "create_spreadsheet"
    }
    fn description(&self) -> &str {
        "Create a real .xlsx workbook at a workspace-relative path -- no external tools required. \
         `sheets` is a list of {\"name\":..., \"headers\":[...], \"rows\":[[...], ...]}; each row \
         cell is either a JSON number or a JSON string. A string cell starting with \"=\" is \
         written as a live Excel formula (e.g. \"=SUM(B2:B10)\"), not literal text. Multiple \
         sheets are written in the given order."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "path",
                    serde_json::json!({"type": "string", "description": "workspace-relative output path, e.g. \"data.xlsx\""}),
                ),
                (
                    "sheets",
                    serde_json::json!({
                        "type": "array",
                        "description": "one entry per worksheet -- see tool description for the exact shape",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "headers": {"type": "array", "items": {"type": "string"}},
                                "rows": {"type": "array", "items": {"type": "array"}},
                            },
                            "required": ["name"],
                        },
                    }),
                ),
            ],
            &["path", "sheets"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let a: XlsxArgs = serde_json::from_str(args.get())?;
        if a.path.is_empty() {
            return Err(ToolError::Message("path is required".into()));
        }
        if a.sheets.is_empty() {
            return Err(ToolError::Message("at least one sheet is required".into()));
        }

        let bytes = build_workbook(&a.sheets).map_err(xlsx_err)?;

        // Same create-parent-dirs-then-resolve dance as `write_file`.
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

        Ok(ToolResult::ok(format!(
            "wrote {} bytes to {} ({} sheet{})",
            bytes.len(),
            a.path,
            a.sheets.len(),
            if a.sheets.len() == 1 { "" } else { "s" }
        )))
    }
}

fn build_workbook(sheets: &[Sheet]) -> Result<Vec<u8>, XlsxError> {
    let mut workbook = Workbook::new();
    let header_format = Format::new().set_bold();

    for sheet in sheets {
        let ws = workbook.add_worksheet();
        // A duplicate/invalid sheet name is a real `XlsxError`, not a panic
        // -- surfaced to the model like any other tool error so it can
        // retry with a fixed name instead of the whole call being lost.
        ws.set_name(&sheet.name)?;

        let mut row: u32 = 0;
        if !sheet.headers.is_empty() {
            for (col, header) in sheet.headers.iter().enumerate() {
                ws.write_string_with_format(row, col as u16, header.as_str(), &header_format)?;
            }
            row += 1;
        }
        for data_row in &sheet.rows {
            for (col, cell) in data_row.iter().enumerate() {
                let col = col as u16;
                match cell {
                    Cell::Number(n) => {
                        ws.write_number(row, col, *n)?;
                    }
                    Cell::Text(s) if s.starts_with('=') => {
                        ws.write_formula(row, col, s.as_str())?;
                    }
                    Cell::Text(s) => {
                        ws.write_string(row, col, s.as_str())?;
                    }
                }
            }
            row += 1;
        }
    }

    workbook.save_to_buffer()
}

fn xlsx_err(e: XlsxError) -> ToolError {
    ToolError::Message(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!(
            "hivemind_xlsx_test_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    #[tokio::test]
    async fn writes_a_valid_xlsx_zip_with_the_expected_sheet_count() {
        let w = ws("basic");
        let out = CreateSpreadsheet(w.clone())
            .execute(&args(serde_json::json!({
                "path": "data.xlsx",
                "sheets": [{
                    "name": "Sales",
                    "headers": ["Product", "Units", "Revenue"],
                    "rows": [["Widget", 10, 250.5], ["Gadget", 3, "=B2*2"]],
                }],
            })))
            .await
            .unwrap()
            .summary;
        assert!(
            out.contains("data.xlsx") && out.contains("1 sheet"),
            "got: {out}"
        );

        // .xlsx is a zip container -- "PK\x03\x04" is the local-file-header
        // magic every valid zip (and therefore every valid xlsx) starts with.
        let bytes = std::fs::read(w.root.join("data.xlsx")).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"), "missing zip/xlsx header");
    }

    #[tokio::test]
    async fn writes_multiple_sheets_in_order() {
        let w = ws("multi");
        let out = CreateSpreadsheet(w.clone())
            .execute(&args(serde_json::json!({
                "path": "multi.xlsx",
                "sheets": [
                    {"name": "First", "headers": ["A"], "rows": [[1]]},
                    {"name": "Second", "headers": ["B"], "rows": [[2]]},
                ],
            })))
            .await
            .unwrap()
            .summary;
        assert!(out.contains("2 sheets"), "got: {out}");
    }

    #[tokio::test]
    async fn rejects_an_empty_sheet_list() {
        let w = ws("no_sheets");
        let err = CreateSpreadsheet(w)
            .execute(&args(serde_json::json!({"path": "x.xlsx", "sheets": []})))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Message(_)));
    }

    #[tokio::test]
    async fn a_duplicate_sheet_name_is_a_tool_error_not_a_panic() {
        let w = ws("dup_names");
        let err = CreateSpreadsheet(w)
            .execute(&args(serde_json::json!({
                "path": "dup.xlsx",
                "sheets": [
                    {"name": "Same", "headers": [], "rows": []},
                    {"name": "Same", "headers": [], "rows": []},
                ],
            })))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Message(_)));
    }
}
