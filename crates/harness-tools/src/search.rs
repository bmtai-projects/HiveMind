//! Read-only, workspace-confined content search — the agent's cheap way to
//! *find* code before it reads or edits it.
//!
//! Without this, the only way to locate a symbol is `run_shell("grep ...")`,
//! which in interactive mode blocks on a `[y/N]` approval prompt, assumes a
//! particular grep/ripgrep is on `PATH`, and pays a full shell round-trip.
//! A first-class search tool needs no approval — it only reads, exactly like
//! `read_file`/`list_dir` — returns `path:line: text` hits directly, and lets
//! the model pinpoint a target in one cheap Flash turn instead of a fan-out
//! of blind `read_file`s. Fewer turns means less re-sent context, which is
//! the whole cost game.
//!
//! This is *exact* substring matching. Its ranked, fuzzy counterpart is
//! [`crate::semantic`]'s `semantic_search`.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::error::ToolError;
use crate::fs::Workspace;
use crate::tool::{Tool, ToolResult, obj_schema};
use crate::walk::walk_files;

const DEFAULT_MAX_RESULTS: usize = 50;
const MAX_RESULTS_CAP: usize = 200;
/// Cap a single result line so one very long line can't blow up the tool
/// result (which is re-sent as input on every later turn until compaction).
const MAX_LINE_CHARS: usize = 240;

/// One typed search match. Keeping this structured inside the tools crate
/// lets composite read-only tools follow a hit without parsing the
/// human-facing `path:line: text` rendering returned by `search`.
#[derive(Debug, Clone)]
pub(crate) struct SearchHit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchOutput {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
}

impl SearchOutput {
    pub(crate) fn summary(&self, query: &str, limit: usize) -> String {
        if self.hits.is_empty() {
            return format!("no matches for {query:?}");
        }

        let mut lines: Vec<String> = self
            .hits
            .iter()
            .map(|hit| format!("{}:{}: {}", hit.path, hit.line, hit.text))
            .collect();
        if self.truncated {
            lines.push(format!(
                "[stopped at {limit} matches — narrow the query or path]"
            ));
        }
        lines.join("\n")
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

/// Literal, case-sensitive content search rooted at the workspace. Read-only,
/// so hosts register it without an approval gate.
pub struct Search(pub Workspace);

#[async_trait]
impl Tool for Search {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Search file contents under the workspace for a literal, case-sensitive substring and \
         return matching `path:line: text` locations. Read-only and fast — use it to locate code \
         before reading or editing, in preference to shell grep. Skips .git/target/node_modules \
         and large or binary files."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "query",
                    serde_json::json!({"type": "string", "description": "literal substring to search for (case-sensitive)"}),
                ),
                (
                    "path",
                    serde_json::json!({"type": "string", "description": "optional workspace-relative file or directory to limit the search to; defaults to the whole workspace"}),
                ),
                (
                    "max_results",
                    serde_json::json!({"type": "integer", "description": "cap on the number of matches returned (default 50)"}),
                ),
            ],
            &["query"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let a: SearchArgs = serde_json::from_str(args.get())?;
        if a.query.is_empty() {
            return Err(ToolError::Message("query is required".into()));
        }
        let limit = a
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, MAX_RESULTS_CAP);

        let output = search_workspace(&self.0, &a.query, a.path.as_deref(), limit).await?;
        Ok(ToolResult::ok(output.summary(&a.query, limit)))
    }
}

/// Run the same workspace-confined search used by the public tool, while
/// retaining typed hits for safe in-process follow-up reads.
pub(crate) async fn search_workspace(
    workspace: &Workspace,
    query: &str,
    path: Option<&str>,
    limit: usize,
) -> Result<SearchOutput, ToolError> {
    let search_root = match path {
        Some(path) if !path.is_empty() => workspace.resolve(path)?,
        _ => workspace.resolve(".")?,
    };
    let strip_base = workspace.resolve(".")?;
    let query = query.to_string();

    tokio::task::spawn_blocking(move || search_tree(&search_root, &strip_base, &query, limit))
        .await
        .map_err(|e| ToolError::Message(format!("search task failed: {e}")))
}

/// Walk `root`, collecting up to `limit` `relpath:line: text` matches for
/// `query`. Paths are reported relative to `strip_base` (the workspace root).
fn search_tree(root: &Path, strip_base: &Path, query: &str, limit: usize) -> SearchOutput {
    let mut hits = Vec::new();
    'files: for path in walk_files(root) {
        // Non-UTF-8 (binary) files fail here and are simply skipped.
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(strip_base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        for (i, line) in contents.lines().enumerate() {
            if line.contains(query) {
                hits.push(SearchHit {
                    path: rel.clone(),
                    line: i + 1,
                    text: truncate_line(line),
                });
                if hits.len() >= limit {
                    break 'files;
                }
            }
        }
    }
    let truncated = hits.len() >= limit;
    SearchOutput { hits, truncated }
}

fn truncate_line(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() > MAX_LINE_CHARS {
        let head: String = line.chars().take(MAX_LINE_CHARS).collect();
        format!("{head}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!(
            "hivemind_search_test_{name}_{}_{:?}",
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
    async fn finds_matches_with_relative_path_and_line_number() {
        let w = ws("finds");
        std::fs::write(w.root.join("a.rs"), "fn main() {}\nlet x = foo();\n").unwrap();
        let out = Search(w.clone())
            .execute(&args(serde_json::json!({"query": "foo"})))
            .await
            .unwrap()
            .summary;
        assert!(out.contains("a.rs:2:"), "got {out:?}");
        assert!(out.contains("foo"), "got {out:?}");
    }

    #[tokio::test]
    async fn reports_no_matches_cleanly() {
        let w = ws("none");
        std::fs::write(w.root.join("a.rs"), "nothing here").unwrap();
        let out = Search(w.clone())
            .execute(&args(serde_json::json!({"query": "zzz"})))
            .await
            .unwrap()
            .summary;
        assert!(out.contains("no matches"), "got {out:?}");
    }

    #[tokio::test]
    async fn skips_ignored_directories() {
        let w = ws("ignore");
        std::fs::create_dir_all(w.root.join("target")).unwrap();
        std::fs::write(w.root.join("target/generated.rs"), "needle").unwrap();
        std::fs::write(w.root.join("keep.rs"), "needle").unwrap();
        let out = Search(w.clone())
            .execute(&args(serde_json::json!({"query": "needle"})))
            .await
            .unwrap()
            .summary;
        assert!(out.contains("keep.rs"), "got {out:?}");
        assert!(!out.contains("target"), "ignored dir leaked: {out:?}");
    }

    #[tokio::test]
    async fn respects_max_results() {
        let w = ws("limit");
        std::fs::write(w.root.join("a.rs"), "hit\n".repeat(10)).unwrap();
        let out = Search(w.clone())
            .execute(&args(serde_json::json!({"query": "hit", "max_results": 3})))
            .await
            .unwrap()
            .summary;
        assert!(out.contains("stopped at 3"), "got {out:?}");
    }

    #[tokio::test]
    async fn can_scope_the_search_to_a_subdirectory() {
        let w = ws("scoped");
        std::fs::create_dir_all(w.root.join("src")).unwrap();
        std::fs::write(w.root.join("src/lib.rs"), "target_symbol").unwrap();
        std::fs::write(w.root.join("top.rs"), "target_symbol").unwrap();
        let out = Search(w.clone())
            .execute(&args(
                serde_json::json!({"query": "target_symbol", "path": "src"}),
            ))
            .await
            .unwrap()
            .summary;
        assert!(out.contains("src/lib.rs"), "got {out:?}");
        assert!(!out.contains("top.rs"), "search escaped its scope: {out:?}");
    }
}
