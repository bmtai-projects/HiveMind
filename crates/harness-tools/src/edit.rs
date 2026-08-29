//! Anchored, in-place file editing — the token-cheap counterpart to
//! `write_file`'s full rewrite.
//!
//! A coding agent spends most of its *output* budget rewriting files it
//! barely changed: to touch three lines of a 400-line file with `write_file`,
//! the model must re-emit all 400 lines, and every one of those is a fresh
//! completion token — the single most expensive token class there is (never
//! cached, billed at the output rate, ~100× a cache-hit prompt token).
//! `edit_file` replaces one exact `old_string` with `new_string`, so the
//! model emits only the span that actually changed: tens of output tokens
//! instead of thousands per edit. That's why the system prompt steers the
//! model here for modifications and keeps `write_file` for brand-new files.
//!
//! It's also *safer*. The model can't accidentally drop, truncate, or reflow
//! code it never re-typed, so edits corrupt less, which means fewer retry
//! turns and less Flash→Pro escalation — a compounding cost win on top of the
//! direct output-token saving.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::error::ToolError;
use crate::fs::Workspace;
use crate::tool::{FileChangeKind, Tool, ToolResult, obj_schema};

/// Unchanged lines shown either side of an edit in the result.
///
/// Enough to re-anchor a following `edit_file` without re-reading, small
/// enough that echoing it back costs a fraction of the turn it saves.
const CONTEXT_PAD_LINES: usize = 12;
/// Hard cap on the echoed window. Replacing a 400-line block should not
/// echo 400 lines back -- past a point the model should read deliberately
/// rather than have the whole thing pushed at it.
const MAX_WINDOW_LINES: usize = 60;

/// The edited region of the file *after* the write, with 1-based line
/// numbers.
///
/// # Why the result carries this at all
///
/// `edit_file` demands an exact `old_string`, and the moment an edit lands
/// the model's picture of that file is stale -- so the only way to get an
/// exact anchor for the *next* edit is to read the file again. Measured on
/// a real session that implemented a tracing feature: 31 `read_file` calls
/// against 24 edits, and 16 of those reads were of a file HiveMind had
/// itself just written. That is 26% of a 60-turn allowance spent
/// re-reading its own output, on the run that then hit the turn cap.
///
/// Echoing the post-edit window costs a few hundred tokens; the read it
/// replaces costs a whole turn plus a round trip. Turns are the scarce
/// resource -- the cap is what stops real work -- so this trade is heavily
/// in favour of spending tokens.
fn post_edit_window(path: &str, updated: &str, start_byte: usize, new_len: usize) -> String {
    let lines: Vec<&str> = updated.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    // Byte offsets -> 0-based line indices. Counting newlines before the
    // offset is exact for both, since `start_byte` is a char boundary that
    // `str::find` returned and `new_len` is the length of the text written
    // at it.
    let first_changed = updated[..start_byte].matches('\n').count();
    let last_changed = updated[..start_byte + new_len].matches('\n').count();

    let from = first_changed.saturating_sub(CONTEXT_PAD_LINES);
    let to = (last_changed + CONTEXT_PAD_LINES).min(lines.len().saturating_sub(1));
    let truncated = to - from + 1 > MAX_WINDOW_LINES;
    let to = if truncated {
        from + MAX_WINDOW_LINES - 1
    } else {
        to
    };

    let width = (to + 1).to_string().len();
    let mut out = format!(
        "\n\n{path} after the edit, lines {}-{}:\n",
        from + 1,
        to + 1
    );
    for (i, line) in lines[from..=to].iter().enumerate() {
        out.push_str(&format!(
            "{:>width$} | {line}\n",
            from + i + 1,
            width = width
        ));
    }
    if truncated {
        out.push_str("[window capped -- read the file if you need more of it]\n");
    }
    out
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

/// In-place substring replacement within one existing workspace file.
/// Confined to the workspace root exactly like the other file tools — it
/// shares [`Workspace::resolve`] for the path-escape check.
pub struct EditFile(pub Workspace);

#[async_trait]
impl Tool for EditFile {
    /// `edit_file` is a read-modify-write (see `execute`), so two edits to
    /// one file *must not* run concurrently -- both would read the same
    /// original and the second write would silently discard the first.
    fn conflict_key(&self, args: &RawValue) -> Option<String> {
        crate::fs::path_conflict_key(&self.0, args)
    }
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Replace an exact substring in an existing file. Prefer this over write_file for modifying \
         files: it rewrites only the changed span, so it is far cheaper and cannot corrupt the \
         parts you leave untouched. `old_string` must match the file exactly (whitespace and \
         indentation included) and, unless `replace_all` is true, must be unique — include enough \
         surrounding context to pin down a single occurrence. On success the result echoes the \
         edited region of the file as it now reads, with line numbers — use that as the anchor for \
         your next edit to the same file instead of reading it again."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "path",
                    serde_json::json!({"type": "string", "description": "workspace-relative file path"}),
                ),
                (
                    "old_string",
                    serde_json::json!({"type": "string", "description": "exact text to find; must be unique unless replace_all is set"}),
                ),
                (
                    "new_string",
                    serde_json::json!({"type": "string", "description": "text to replace it with"}),
                ),
                (
                    "replace_all",
                    serde_json::json!({"type": "boolean", "description": "replace every occurrence instead of requiring a unique match (default false)"}),
                ),
            ],
            &["path", "old_string", "new_string"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let a: EditArgs = serde_json::from_str(args.get())?;
        if a.old_string.is_empty() {
            return Err(ToolError::Message(
                "old_string is empty — use write_file to create a file or replace its whole contents"
                    .into(),
            ));
        }
        if a.old_string == a.new_string {
            return Err(ToolError::Message(
                "old_string and new_string are identical — nothing to change".into(),
            ));
        }

        let p = self.0.resolve(&a.path)?;
        let original = tokio::fs::read_to_string(&p)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => ToolError::Message(format!(
                    "{}: no such file — use write_file to create it",
                    a.path
                )),
                _ => ToolError::Io(e),
            })?;

        // Checked before the match, not after: when a file has been
        // rewritten underneath the agent, "this file moved under you" is a
        // more useful thing to hear than "your old_string wasn't found",
        // and it points at a different fix (re-read, don't re-guess).
        if self.0.read_set.is_stale(&p, &original) {
            return Err(ToolError::Message(format!(
                "{} changed since you last read it -- something else (a formatter, the user, \
                 another process) has written to it. Read it again before editing, so the edit \
                 is based on what the file actually contains now.",
                a.path
            )));
        }

        let occurrences = original.matches(&a.old_string).count();
        if occurrences == 0 {
            return Err(ToolError::Message(format!(
                "old_string not found in {} — read the file and copy the exact text, whitespace included",
                a.path
            )));
        }
        if occurrences > 1 && !a.replace_all {
            return Err(ToolError::Message(format!(
                "old_string matches {occurrences} places in {} — add surrounding context to make it unique, or pass replace_all=true",
                a.path
            )));
        }

        // Where the replacement lands. `occurrences >= 1` is guaranteed
        // above, so this never misses; for `replace_all` it is the first
        // of several sites, which is the one worth echoing.
        let start_byte = original.find(&a.old_string).unwrap_or(0);
        let updated = if a.replace_all {
            original.replace(&a.old_string, &a.new_string)
        } else {
            // Exactly one occurrence here, so replacen(_, _, 1) == replace,
            // but it states the intent: a unique-match edit touches one site.
            original.replacen(&a.old_string, &a.new_string, 1)
        };
        tokio::fs::write(&p, &updated).await?;
        // Re-fingerprint to what we just wrote, so a second edit to this
        // same file doesn't see the first edit as interference.
        self.0.read_set.record(&p, &updated);

        let n = if a.replace_all { occurrences } else { 1 };
        let mut out = format!(
            "edited {} ({n} replacement{})",
            a.path,
            if n == 1 { "" } else { "s" }
        );
        // Only the text being introduced is scanned, never the whole file:
        // an unrelated credential already sitting in the file would
        // otherwise re-fire on every edit forever, which is exactly how a
        // warning becomes background noise nobody reads.
        if let Some(note) = crate::secrets::warning(&crate::secrets::scan(&a.new_string)) {
            out.push_str(&note);
        }
        // Only for a single-site edit. With `replace_all` the sites are
        // scattered and one window would describe the file misleadingly --
        // showing the first while implying it covers all of them.
        if n == 1 {
            out.push_str(&post_edit_window(
                &a.path,
                &updated,
                start_byte,
                a.new_string.len(),
            ));
        }
        // `edit_file` only ever touches a file that already existed -- it
        // errors out above when the path is missing -- so this is always a
        // modification, never a creation.
        Ok(ToolResult::ok(out).with_changed_file(&a.path, FileChangeKind::Modified))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!(
            "hivemind_edit_test_{name}_{}_{:?}",
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

    /// The measured regression this exists for: after an edit the model has
    /// no current text for the file, so it reads again to get an exact
    /// `old_string`. One session spent 16 of 60 turns re-reading files it
    /// had itself just written.
    #[tokio::test]
    async fn a_successful_edit_returns_the_new_text_so_the_next_edit_needs_no_re_read() {
        let w = ws("returns_context");
        let body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(w.root.join("f.rs"), &body).unwrap();

        let out = EditFile(w)
            .execute(&args(serde_json::json!({
                "path": "f.rs",
                "old_string": "line 20",
                "new_string": "line 20 CHANGED",
            })))
            .await
            .unwrap()
            .summary;

        assert!(out.starts_with("edited f.rs (1 replacement)"), "{out}");
        assert!(out.contains("f.rs after the edit"), "{out}");
        // The edited line, as it now reads on disk -- an exact anchor.
        assert!(out.contains("line 20 CHANGED"), "{out}");
        // Surrounding context, so a following edit can anchor near it too.
        assert!(out.contains("line 12"), "{out}");
        assert!(out.contains("line 28"), "{out}");
        // Numbered, so `read_file` with offset/limit stays usable.
        assert!(out.contains("20 | line 20 CHANGED"), "{out}");
        // Not the whole file.
        assert!(
            !out.contains("line 1 \n") && !out.contains("| line 40"),
            "{out}"
        );
    }

    /// An edit near the top must not underflow into a negative window.
    #[tokio::test]
    async fn an_edit_on_the_first_line_still_renders() {
        let w = ws("first_line");
        std::fs::write(w.root.join("f.rs"), "alpha\nbeta\ngamma\n").unwrap();
        let out = EditFile(w)
            .execute(&args(serde_json::json!({
                "path": "f.rs", "old_string": "alpha", "new_string": "ALPHA",
            })))
            .await
            .unwrap()
            .summary;
        assert!(out.contains("lines 1-3"), "{out}");
        assert!(out.contains("1 | ALPHA"), "{out}");
    }

    /// Replacing a large block must not echo the block back -- past a point
    /// the saving inverts and the result is just expensive.
    #[tokio::test]
    async fn a_huge_replacement_is_capped_rather_than_echoed_whole() {
        let w = ws("capped");
        let body: String = (1..=400).map(|i| format!("line {i}\n")).collect();
        std::fs::write(w.root.join("f.rs"), &body).unwrap();
        let big: String = (1..=300).map(|i| format!("new {i}\n")).collect();

        let out = EditFile(w)
            .execute(&args(serde_json::json!({
                "path": "f.rs",
                "old_string": "line 100\n",
                "new_string": big,
            })))
            .await
            .unwrap()
            .summary;

        let shown = out.lines().filter(|l| l.contains(" | ")).count();
        assert!(shown <= MAX_WINDOW_LINES, "echoed {shown} lines");
        assert!(out.contains("window capped"), "{out}");
    }

    /// With `replace_all` the sites are scattered; one window would describe
    /// the file misleadingly, so none is shown.
    #[tokio::test]
    async fn replace_all_reports_the_count_and_shows_no_window() {
        let w = ws("replace_all_nowindow");
        std::fs::write(w.root.join("f.rs"), "x\ny\nx\n").unwrap();
        let out = EditFile(w)
            .execute(&args(serde_json::json!({
                "path": "f.rs", "old_string": "x", "new_string": "z", "replace_all": true,
            })))
            .await
            .unwrap()
            .summary;
        assert!(out.contains("2 replacements"), "{out}");
        assert!(!out.contains("after the edit"), "{out}");
    }

    #[tokio::test]
    async fn replaces_a_unique_occurrence() {
        let w = ws("unique");
        std::fs::write(w.root.join("a.txt"), "hello world").unwrap();
        let out = EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "a.txt", "old_string": "world", "new_string": "there"
            })))
            .await
            .unwrap();
        assert!(out.summary.contains("1 replacement"), "got {out:?}");
        assert_eq!(
            std::fs::read_to_string(w.root.join("a.txt")).unwrap(),
            "hello there"
        );
    }

    #[tokio::test]
    async fn ambiguous_match_without_replace_all_is_rejected_and_file_untouched() {
        let w = ws("ambiguous");
        std::fs::write(w.root.join("a.txt"), "x x x").unwrap();
        let err = EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "a.txt", "old_string": "x", "new_string": "y"
            })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("3 places"), "got {err}");
        assert_eq!(
            std::fs::read_to_string(w.root.join("a.txt")).unwrap(),
            "x x x",
            "a rejected edit must not modify the file"
        );
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let w = ws("all");
        std::fs::write(w.root.join("a.txt"), "x x x").unwrap();
        let out = EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true
            })))
            .await
            .unwrap();
        assert!(out.summary.contains("3 replacements"), "got {out:?}");
        assert_eq!(
            std::fs::read_to_string(w.root.join("a.txt")).unwrap(),
            "y y y"
        );
    }

    #[tokio::test]
    async fn missing_old_string_is_an_error() {
        let w = ws("missing_old");
        std::fs::write(w.root.join("a.txt"), "hello").unwrap();
        let err = EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "a.txt", "old_string": "zzz", "new_string": "q"
            })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "got {err}");
    }

    #[tokio::test]
    async fn editing_a_missing_file_points_at_write_file() {
        let w = ws("missing_file");
        let err = EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "nope.txt", "old_string": "a", "new_string": "b"
            })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("write_file"), "got {err}");
    }

    #[tokio::test]
    async fn identical_old_and_new_is_rejected() {
        let w = ws("noop");
        std::fs::write(w.root.join("a.txt"), "hello").unwrap();
        let err = EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "a.txt", "old_string": "hello", "new_string": "hello"
            })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("identical"), "got {err}");
    }

    #[tokio::test]
    async fn escaping_the_workspace_is_rejected_and_the_outside_file_is_untouched() {
        // A real file just outside the workspace root, reached via `../`.
        //
        // The target has to live outside the workspace, which puts it in the
        // shared temp directory -- so unlike every other fixture here it
        // cannot be isolated by the workspace name, and needs its own unique
        // filename. A fixed one is shared by any two concurrent test runs,
        // which then delete it from under each other.
        let w = ws("escape");
        let target_name = format!(
            "hivemind_edit_escape_target_{}_{:?}.txt",
            std::process::id(),
            std::thread::current().id()
        );
        let outside = w.root.parent().unwrap().join(&target_name);
        std::fs::write(&outside, "secret").unwrap();

        let err = EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": format!("../{target_name}"),
                "old_string": "secret",
                "new_string": "leaked"
            })))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("escapes"), "got {err}");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "secret",
            "edit_file must never write outside the workspace root"
        );
        let _ = std::fs::remove_file(&outside);
    }
}

/// Integration of the read-set staleness guard through the real tools --
/// `ReadSet`'s own unit tests live in `crate::readset`, but the wiring
/// between `read_file`, `write_file` and `edit_file` is what actually has
/// to hold, and it spans three files.
#[cfg(test)]
mod staleness_tests {
    use super::*;
    use crate::fs::{ReadFile, WriteFile};

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!(
            "hivemind_stale_test_{name}_{}_{:?}",
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

    /// Read for the side effect only -- this is what puts the file in the
    /// read-set so a following `edit_file` isn't rejected as stale.
    async fn read(w: &Workspace, path: &str) {
        ReadFile(w.clone())
            .execute(&args(serde_json::json!({ "path": path })))
            .await
            .unwrap();
    }

    async fn edit(w: &Workspace, path: &str, old: &str, new: &str) -> Result<String, ToolError> {
        EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": path, "old_string": old, "new_string": new
            })))
            .await
            .map(|r| r.summary)
    }

    #[tokio::test]
    async fn editing_a_file_nobody_read_is_still_allowed() {
        // Finding a file via `search` or `project_map` and editing it
        // without a separate read is a normal flow. Blocking it would be a
        // regression, not a safety feature.
        let w = ws("unread");
        std::fs::write(w.root.join("a.txt"), "hello world").unwrap();
        assert!(edit(&w, "a.txt", "world", "there").await.is_ok());
    }

    #[tokio::test]
    async fn read_then_edit_works_normally() {
        let w = ws("normal");
        std::fs::write(w.root.join("a.txt"), "hello world").unwrap();
        read(&w, "a.txt").await;
        assert!(edit(&w, "a.txt", "world", "there").await.is_ok());
    }

    #[tokio::test]
    async fn an_edit_after_someone_else_rewrote_the_file_is_refused() {
        let w = ws("rewritten");
        std::fs::write(w.root.join("a.txt"), "hello world").unwrap();
        read(&w, "a.txt").await;
        // Someone else touches the file *outside* the region being edited,
        // so `old_string` still matches -- exactly the case the exact-match
        // requirement cannot catch on its own.
        std::fs::write(w.root.join("a.txt"), "// added by a formatter\nhello world").unwrap();

        let err = edit(&w, "a.txt", "world", "there")
            .await
            .expect_err("a stale edit must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("changed since you last read it"),
            "got {msg:?}"
        );
        assert!(msg.contains("Read it again"), "must say how to recover");

        // And the refusal must be a no-op on disk, not a partial write.
        assert_eq!(
            std::fs::read_to_string(w.root.join("a.txt")).unwrap(),
            "// added by a formatter\nhello world"
        );
    }

    #[tokio::test]
    async fn two_consecutive_edits_to_one_file_both_succeed() {
        // The regression this guards: if the first edit didn't re-record,
        // the second would see the agent's own change as interference and
        // refuse, breaking an entirely ordinary sequence.
        let w = ws("consecutive");
        std::fs::write(w.root.join("a.txt"), "one two three").unwrap();
        read(&w, "a.txt").await;
        assert!(edit(&w, "a.txt", "one", "1").await.is_ok());
        assert!(
            edit(&w, "a.txt", "three", "3").await.is_ok(),
            "the first edit's own write must not read as someone else's"
        );
        assert_eq!(
            std::fs::read_to_string(w.root.join("a.txt")).unwrap(),
            "1 two 3"
        );
    }

    #[tokio::test]
    async fn write_then_edit_is_guarded_too() {
        let w = ws("write_then_edit");
        WriteFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "a.txt", "content": "generated content"
            })))
            .await
            .unwrap();
        // Clean edit right after our own write: fine.
        assert!(edit(&w, "a.txt", "generated", "produced").await.is_ok());
        // Now something else clobbers it -- that must be caught even though
        // the agent learned the contents by writing rather than reading.
        std::fs::write(w.root.join("a.txt"), "produced content\n// clobbered").unwrap();
        assert!(edit(&w, "a.txt", "produced", "made").await.is_err());
    }

    #[tokio::test]
    async fn re_reading_a_changed_file_clears_the_refusal() {
        // The recovery path the error message tells the model to take has
        // to actually work.
        let w = ws("recovery");
        std::fs::write(w.root.join("a.txt"), "hello world").unwrap();
        read(&w, "a.txt").await;
        std::fs::write(w.root.join("a.txt"), "prefix\nhello world").unwrap();
        assert!(edit(&w, "a.txt", "world", "there").await.is_err());

        read(&w, "a.txt").await; // do what the error said
        assert!(
            edit(&w, "a.txt", "world", "there").await.is_ok(),
            "re-reading must actually unblock the edit"
        );
    }
}
