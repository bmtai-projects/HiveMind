use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::error::ToolError;
use crate::readset::ReadSet;
use crate::tool::{FileChangeKind, Tool, ToolResult, obj_schema};

/// Confines file tools to a root directory. All paths are resolved relative
/// to `root` and may not escape it.
///
/// Also carries the session's [`ReadSet`]. Every file tool is built from a
/// clone of one `Workspace`, so hanging the read tracking here is what lets
/// `read_file` and `edit_file` — separate tools that never talk to each
/// other — share a view of what has been seen. Cloning shares that view
/// rather than copying it.
#[derive(Clone, Default)]
pub struct Workspace {
    pub root: PathBuf,
    pub read_set: ReadSet,
}

/// Shared [`crate::tool::Tool::conflict_key`] implementation for every tool
/// that writes to a single `path` argument. Keys on the *canonical*
/// resolved path, so two calls naming one file differently (`./a.txt` vs
/// `a.txt`) still collide and get serialized rather than racing.
///
/// `None` when the args don't parse or the path escapes the workspace: such
/// a call fails on its own during `execute`, so it can never win a write
/// race it wasn't going to participate in.
pub(crate) fn path_conflict_key(ws: &Workspace, args: &RawValue) -> Option<String> {
    #[derive(Deserialize)]
    struct PathOnly {
        path: String,
    }
    let parsed: PathOnly = serde_json::from_str(args.get()).ok()?;
    let resolved = ws.resolve(&parsed.path).ok()?;
    Some(resolved.to_string_lossy().into_owned())
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            read_set: ReadSet::new(),
        }
    }

    /// Resolve `rel` against the workspace root, rejecting anything that
    /// escapes it. `pub` so hosts (e.g. the CLI's `@file` mention expansion)
    /// can reuse the same path-safety check instead of re-implementing it.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, ToolError> {
        if rel.is_empty() {
            return Err(ToolError::Message("path is required".into()));
        }
        let candidate = Path::new(rel);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };

        let root_abs = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        // The target may not exist yet (write_file creating a new file), so
        // canonicalize its parent instead and re-attach the file name.
        let joined_abs = match joined.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                let parent = joined.parent().unwrap_or(&joined);
                let parent_abs = parent.canonicalize().map_err(ToolError::Io)?;
                match joined.file_name() {
                    Some(name) => parent_abs.join(name),
                    None => parent_abs,
                }
            }
        };

        if joined_abs != root_abs && !joined_abs.starts_with(&root_abs) {
            return Err(ToolError::Message(format!(
                "path {rel:?} escapes the workspace root"
            )));
        }
        Ok(joined_abs)
    }
}

const MAX_READ_BYTES: usize = 60_000;

#[derive(Deserialize)]
struct PathArgs {
    path: String,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    /// 1-indexed first line to return. `None` starts at the top.
    #[serde(default)]
    offset: Option<usize>,
    /// How many lines to return from `offset`. `None` reads to the end (or
    /// to the byte cap).
    #[serde(default)]
    limit: Option<usize>,
}

/// Longest prefix of `s` that is at most `max` bytes and ends on a `char`
/// boundary. Slicing a `&str` mid-codepoint panics, and a minified bundle is
/// exactly the kind of file that puts a multi-byte character across the cap.
fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The lines `read_file` should return, plus a trailing note whenever the
/// model is *not* seeing the whole file.
///
/// The note is the load-bearing part. Without it a partial read is
/// indistinguishable from a complete one, and a model that thinks it has
/// read a whole file will confidently answer from the part it happened to
/// get -- worse than the cost problem partial reads exist to solve.
fn slice_file(text: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    // Fast path, byte-identical to what this tool has always returned: a
    // whole small file, no range requested, no note appended. Whatever the
    // model copies out of it for `edit_file`'s exact-match `old_string` is
    // exactly what is on disk, trailing newline and all.
    if offset.is_none() && limit.is_none() && text.len() <= MAX_READ_BYTES {
        return text.to_string();
    }

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return "[empty file]".to_string();
    }

    // `offset: 0` is a 1-indexing slip, not a request for a phantom line
    // before the first one; treat it as the top rather than erroring on
    // something with an obvious intent.
    let start = offset.unwrap_or(1).max(1);
    if start > total {
        return format!("[offset {start} is past the end of the file: {total} line(s) total]");
    }

    let start_idx = start - 1;
    let want_end = match limit {
        Some(n) => start_idx.saturating_add(n).min(total),
        None => total,
    };

    // The byte cap still applies to an explicit range: a 500-line slice of a
    // minified file can be larger than the whole of an ordinary one. Cutting
    // on a line boundary (rather than mid-line at a byte offset, as this
    // used to) means the last line the model sees is a complete one, and the
    // resume point below is exact.
    let mut out = String::new();
    let mut end_idx = start_idx;
    while end_idx < want_end {
        let line = lines[end_idx];
        let added = line.len() + usize::from(!out.is_empty());
        if !out.is_empty() && out.len() + added > MAX_READ_BYTES {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        end_idx += 1;
        if out.len() >= MAX_READ_BYTES {
            // One line on its own over the cap. Returning nothing would be
            // useless, so hand back as much of it as fits.
            out = truncate_at_char_boundary(&out, MAX_READ_BYTES).to_string();
            break;
        }
    }

    let mut note = format!("\n\n[lines {start}-{end_idx} of {total}");
    if end_idx < total {
        note.push_str(&format!("; read on with offset={}", end_idx + 1));
    }
    note.push(']');
    out.push_str(&note);
    out
}

pub struct ReadFile(pub Workspace);

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a text file at a workspace-relative path. Reads the whole file by default. For a \
         large file, pass `offset` (1-indexed first line) and `limit` (line count) to read only \
         the part you need -- the whole content of a big file is billed as input tokens on every \
         later turn of the session, so reading 80 relevant lines instead of 1500 is much cheaper \
         and just as useful. `project_map` gives you the line numbers to aim at. Every result \
         says which lines you got and how many the file has, so you can read on from there."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "path",
                    serde_json::json!({"type": "string", "description": "workspace-relative file path"}),
                ),
                (
                    "offset",
                    serde_json::json!({
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-indexed line to start at. Omit to start at the top.",
                    }),
                ),
                (
                    "limit",
                    serde_json::json!({
                        "type": "integer",
                        "minimum": 1,
                        "description": "how many lines to read from offset. Omit to read to the end.",
                    }),
                ),
            ],
            &["path"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let a: ReadArgs = serde_json::from_str(args.get())?;
        let p = self.0.resolve(&a.path)?;
        let bytes = tokio::fs::read(&p).await?;
        let text = String::from_utf8_lossy(&bytes);
        // Fingerprint the *whole* file, on every path through this function:
        // the question this answers later is "did this file change since we
        // looked at it", which a slice can't answer. That the model saw only
        // part of it is a separate matter, unaffected either way.
        self.0.read_set.record(&p, &text);
        Ok(ToolResult::ok(slice_file(&text, a.offset, a.limit)))
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub struct WriteFile(pub Workspace);

#[async_trait]
impl Tool for WriteFile {
    fn conflict_key(&self, args: &RawValue) -> Option<String> {
        path_conflict_key(&self.0, args)
    }
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create or overwrite a text file at the given workspace-relative path with the provided content."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "path",
                    serde_json::json!({"type": "string", "description": "workspace-relative file path"}),
                ),
                (
                    "content",
                    serde_json::json!({"type": "string", "description": "full file content to write"}),
                ),
            ],
            &["path", "content"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let a: WriteArgs = serde_json::from_str(args.get())?;
        if a.path.is_empty() {
            return Err(ToolError::Message("path is required".into()));
        }
        let candidate = Path::new(&a.path);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.0.root.join(candidate)
        };
        if let Some(parent) = joined.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Re-resolve now that the parent exists, to enforce the workspace boundary.
        let p = self.0.resolve(&a.path)?;
        // Checked before the write, because afterwards everything exists.
        let existed = tokio::fs::try_exists(&p).await.unwrap_or(false);
        tokio::fs::write(&p, &a.content).await?;
        // A tool that just wrote the file knows exactly what's in it, so
        // this counts as having seen it -- otherwise a write-then-edit
        // sequence would leave the edit unguarded.
        self.0.read_set.record(&p, &a.content);

        let mut out = format!("wrote {} bytes to {}", a.content.len(), a.path);
        // Appended to a *successful* result rather than raised as an error:
        // the write is legitimate far more often than not (fixtures, docs,
        // examples), so this informs without blocking. See `crate::secrets`.
        if let Some(note) = crate::secrets::warning(&crate::secrets::scan(&a.content)) {
            out.push_str(&note);
        }
        Ok(ToolResult::ok(out).with_changed_file(
            &a.path,
            if existed {
                FileChangeKind::Modified
            } else {
                FileChangeKind::Created
            },
        ))
    }
}

pub struct ListDir(pub Workspace);

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List the entries of a directory (workspace-relative). Directories are suffixed with '/'."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[(
                "path",
                serde_json::json!({"type": "string", "description": "workspace-relative directory path; '.' for root"}),
            )],
            &["path"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let mut a: PathArgs = serde_json::from_str(args.get())?;
        if a.path.is_empty() {
            a.path = ".".to_string();
        }
        let p = self.0.resolve(&a.path)?;
        let mut entries = tokio::fs::read_dir(&p).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                name.push('/');
            }
            names.push(name);
        }
        names.sort();
        if names.is_empty() {
            return Ok(ToolResult::ok("(empty)".to_string()));
        }
        Ok(ToolResult::ok(names.join("\n")))
    }
}

#[cfg(test)]
mod read_slice_tests {
    use super::*;
    use crate::tool::ToolStatus;

    fn numbered(n: usize) -> String {
        (1..=n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_small_whole_file_is_returned_byte_identical_with_no_note() {
        // The exactness guarantee `edit_file` depends on: whatever the model
        // copies out of a full read as `old_string` must be what is on disk,
        // trailing newline included.
        let text = "fn main() {\n    println!(\"hi\");\n}\n";
        assert_eq!(slice_file(text, None, None), text);
    }

    #[test]
    fn an_explicit_range_returns_only_those_lines() {
        let out = slice_file(&numbered(100), Some(10), Some(3));
        assert!(out.starts_with("line 10\nline 11\nline 12"), "{out}");
        assert!(
            !out.contains("line 13"),
            "limit must be exclusive of the next line"
        );
        assert!(
            !out.contains("line 9"),
            "offset must not include the line before"
        );
    }

    #[test]
    fn a_partial_read_says_what_it_showed_and_where_to_resume() {
        // Without this a partial read looks exactly like a complete one, and
        // the model answers confidently from whatever slice it happened to
        // get -- a worse failure than the cost this feature exists to avoid.
        let out = slice_file(&numbered(100), Some(10), Some(3));
        assert!(out.contains("[lines 10-12 of 100"), "{out}");
        assert!(out.contains("read on with offset=13"), "{out}");
    }

    #[test]
    fn reaching_the_last_line_offers_no_resume_point() {
        let out = slice_file(&numbered(20), Some(18), Some(5));
        assert!(out.contains("[lines 18-20 of 20]"), "{out}");
        assert!(
            !out.contains("read on"),
            "there is nothing left to read: {out}"
        );
    }

    #[test]
    fn a_limit_past_the_end_clamps_instead_of_erroring() {
        let out = slice_file(&numbered(5), Some(3), Some(999));
        assert!(out.contains("line 5"));
        assert!(out.contains("[lines 3-5 of 5]"), "{out}");
    }

    #[test]
    fn an_offset_past_the_end_says_so_rather_than_returning_nothing() {
        // Returning "" here would read as an empty file and send the model
        // off explaining why the file is blank.
        let out = slice_file(&numbered(5), Some(50), None);
        assert!(out.contains("past the end"), "{out}");
        assert!(out.contains("5 line(s)"), "{out}");
    }

    #[test]
    fn offset_zero_is_treated_as_the_first_line() {
        // A 1-indexing slip with obvious intent; erroring on it just costs a
        // turn to correct.
        let out = slice_file(&numbered(10), Some(0), Some(2));
        assert!(out.starts_with("line 1\nline 2"), "{out}");
    }

    #[test]
    fn an_empty_file_is_labelled_not_returned_as_blank() {
        assert_eq!(slice_file("", Some(1), None), "[empty file]");
    }

    #[test]
    fn an_oversized_file_cuts_on_a_line_boundary_and_gives_a_resume_point() {
        let big = numbered(20_000); // well past MAX_READ_BYTES
        let out = slice_file(&big, None, None);
        assert!(
            out.len() < MAX_READ_BYTES + 200,
            "must respect the cap: {}",
            out.len()
        );
        let body = out.split("\n\n[lines").next().unwrap();
        assert!(
            body.lines().last().unwrap().starts_with("line "),
            "the last line shown must be a whole one, not cut mid-token",
        );
        assert!(
            out.contains("read on with offset="),
            "{}",
            &out[out.len() - 80..]
        );
    }

    #[test]
    fn a_multibyte_character_across_the_cap_does_not_panic() {
        // Slicing a &str mid-codepoint panics, and a minified bundle is
        // exactly the file that puts one across the boundary.
        let one_huge_line = "é".repeat(MAX_READ_BYTES);
        let out = slice_file(&one_huge_line, None, None);
        assert!(
            out.contains("[lines 1-1 of 1]"),
            "{}",
            &out[out.len().saturating_sub(40)..]
        );
    }

    #[test]
    fn a_single_line_over_the_cap_still_returns_content() {
        let one_huge_line = "x".repeat(MAX_READ_BYTES * 2);
        let out = slice_file(&one_huge_line, None, None);
        assert!(out.len() > 1000, "returning nothing would be useless");
        assert!(out.len() <= MAX_READ_BYTES + 200);
    }

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!(
            "hivemind_readfile_test_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    fn args(v: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(v.to_string()).unwrap()
    }

    #[tokio::test]
    async fn write_file_reports_creation_and_modification_distinctly() {
        // The distinction is the point: a checkpoint has to delete a file
        // the agent created but restore one it modified. Deriving that from
        // the arguments after the fact is impossible -- by then the file
        // exists either way.
        let w = ws("changed_files");
        let out = WriteFile(w.clone())
            .execute(&args(
                serde_json::json!({"path": "new.txt", "content": "hello"}),
            ))
            .await
            .unwrap();
        assert_eq!(out.changed_files.len(), 1);
        assert_eq!(out.changed_files[0].path, "new.txt");
        assert_eq!(out.changed_files[0].kind, FileChangeKind::Created);

        let out = WriteFile(w.clone())
            .execute(&args(
                serde_json::json!({"path": "new.txt", "content": "goodbye"}),
            ))
            .await
            .unwrap();
        assert_eq!(
            out.changed_files[0].kind,
            FileChangeKind::Modified,
            "the same path a second time is a modification, not a creation"
        );
    }

    #[tokio::test]
    async fn a_read_reports_no_file_changes() {
        let w = ws("no_changes");
        std::fs::write(w.root.join("f.txt"), "x").unwrap();
        let out = ReadFile(w.clone())
            .execute(&args(serde_json::json!({"path": "f.txt"})))
            .await
            .unwrap();
        assert!(out.changed_files.is_empty(), "reading changes nothing");
        assert_eq!(out.status, ToolStatus::Ok);
    }

    #[tokio::test]
    async fn the_tool_itself_honours_offset_and_limit_from_json_args() {
        // slice_file is covered above; this pins the seam in between --
        // that the new fields actually deserialize off the wire rather
        // than being silently dropped into their defaults.
        let w = ws("args");
        std::fs::write(w.root.join("f.txt"), numbered(500)).unwrap();

        let out = ReadFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "f.txt", "offset": 100, "limit": 2
            })))
            .await
            .unwrap()
            .summary;

        assert!(out.starts_with("line 100\nline 101"), "{out}");
        assert!(
            out.contains("[lines 100-101 of 500; read on with offset=102]"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn omitting_the_new_fields_still_reads_the_whole_file() {
        // Backward compatibility: every existing call site passes only
        // `path`, and must keep getting exactly what it always did.
        let w = ws("compat");
        let body = "alpha\nbeta\n";
        std::fs::write(w.root.join("f.txt"), body).unwrap();

        let out = ReadFile(w.clone())
            .execute(&args(serde_json::json!({"path": "f.txt"})))
            .await
            .unwrap()
            .summary;

        assert_eq!(
            out, body,
            "a plain read must be byte-identical, notes included"
        );
    }

    #[tokio::test]
    async fn a_partial_read_still_fingerprints_the_whole_file() {
        // The staleness guard asks "did this change since we looked?", which
        // a fingerprint of one slice cannot answer -- reading lines 1-2 and
        // then editing line 900 must not look like a stale edit.
        let w = ws("fingerprint");
        std::fs::write(w.root.join("f.txt"), numbered(1000)).unwrap();

        ReadFile(w.clone())
            .execute(&args(
                serde_json::json!({"path": "f.txt", "offset": 1, "limit": 2}),
            ))
            .await
            .unwrap();

        let p = w.resolve("f.txt").unwrap();
        assert!(
            !w.read_set.is_stale(&p, &numbered(1000)),
            "the whole file must be recorded, not just the slice shown",
        );
        assert!(
            w.read_set.is_stale(&p, &numbered(999)),
            "a real change must still register as stale",
        );
    }

    #[test]
    fn the_saving_this_exists_for_is_real() {
        // The measured case: main.rs, 1464 lines, read whole to answer a
        // question that needed the clap definitions.
        let whole = numbered(1464);
        let full = slice_file(&whole, None, None);
        let targeted = slice_file(&whole, Some(200), Some(80));
        assert!(
            targeted.len() * 10 < full.len(),
            "a targeted read must be an order of magnitude smaller: {} vs {}",
            targeted.len(),
            full.len(),
        );
    }
}
