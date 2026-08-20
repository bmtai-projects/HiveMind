//! Turn-level undo: snapshot every file `edit_file`/`write_file` touches
//! before a user turn runs, so `/undo` can put both the workspace and the
//! conversation back exactly where they were.
//!
//! Deliberately scoped down from what a full rewind system could be:
//! in-memory only (no disk persistence), no redo, and no coverage of
//! `run_shell` — a shell command's effects are unbounded and can't be
//! captured this way without a much heavier mechanism (e.g. a shadow git
//! commit of the whole tree).

use std::path::PathBuf;

use harness_tools::Workspace;
use harness_types::{Message, ToolCall};
use serde::Deserialize;

/// Cap on how many turns back `/undo` can reach. An in-memory-only design
/// needs a smaller bound than a durable, multi-session store would: the
/// real cost here is holding full file contents as strings for the whole
/// REPL session's lifetime, not disk space.
pub const MAX_CHECKPOINTS: usize = 20;

/// The tool names that participate in checkpointing. Matches grok-build's
/// own scope exactly: shell-driven file changes aren't checkpointed there
/// either — only the dedicated file-edit tools.
///
/// Defined in `validation` and shared rather than kept per-module: the same
/// list decides what `/undo` can restore and what has to be checked before
/// a run ends, and a tool added to one copy but not the other would get
/// half of that silently.
use crate::validation::MUTATING_TOOLS;

#[derive(Deserialize)]
struct PathOnly {
    path: String,
}

/// A file's content immediately before the turn that's about to run.
/// `before: None` means the file didn't exist yet — restoring it means
/// deleting whatever the turn created.
struct FileSnapshot {
    path: PathBuf,
    before: Option<String>,
}

/// Everything needed to undo one user turn: where the conversation was
/// before it started, and every file it's about to touch (or did touch,
/// once the turn completes) in its pre-turn state.
pub struct Checkpoint {
    /// The user's input for this turn, for the `/undo` confirmation
    /// message. Not used for restore logic.
    label: String,
    message_len_before: usize,
    files: Vec<FileSnapshot>,
}

impl Checkpoint {
    /// Opens a checkpoint for a new turn. `message_len_before` should be
    /// `Agent.messages.len()` *before* the user's input is pushed.
    pub fn open(label: &str, message_len_before: usize) -> Self {
        // Labels only ever appear in a one-line confirmation message;
        // truncate so a long paste doesn't wall of text the terminal.
        const MAX_LABEL_CHARS: usize = 60;
        let label = if label.chars().count() > MAX_LABEL_CHARS {
            format!(
                "{}…",
                label.chars().take(MAX_LABEL_CHARS).collect::<String>()
            )
        } else {
            label.to_string()
        };
        Self {
            label,
            message_len_before,
            files: Vec::new(),
        }
    }

    /// Inspects one turn's batch of tool calls and, for any `edit_file`/
    /// `write_file` call whose path hasn't already been captured in this
    /// checkpoint, records that path's current on-disk content (or its
    /// absence) *before* the batch is dispatched.
    ///
    /// Snapshotting a file whose edit then fails validation and never
    /// actually writes is harmless -- just one wasted read in that case --
    /// and is the accepted tradeoff against the complexity of only
    /// snapshotting after confirming success under concurrent dispatch.
    pub async fn capture(&mut self, workspace: &Workspace, calls: &[ToolCall]) {
        for call in calls {
            if !MUTATING_TOOLS.contains(&call.name.as_str()) {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<PathOnly>(call.args.get()) else {
                continue;
            };
            let Ok(resolved) = workspace.resolve(&parsed.path) else {
                continue;
            };
            if self.files.iter().any(|f| f.path == resolved) {
                continue; // first-touch-wins: already have this turn's pre-state
            }
            let before = tokio::fs::read_to_string(&resolved).await.ok();
            self.files.push(FileSnapshot {
                path: resolved,
                before,
            });
        }
    }
}

/// One file the session has touched, paired with how it looked before the
/// session touched it. `before: None` means the session created it.
pub struct OriginalState {
    pub path: PathBuf,
    pub before: Option<String>,
}

/// Every file the session has changed, each paired with its state *before
/// the earliest turn that touched it* — which is what "what has this
/// session done to my workspace" actually means.
///
/// Walks oldest checkpoint first and keeps the first snapshot seen per
/// path, mirroring the first-touch-wins rule inside a single checkpoint.
/// Taking the newest instead would describe only the last turn's edit and
/// silently hide everything before it.
///
/// Bounded by the same [`MAX_CHECKPOINTS`] window as `/undo`: a file
/// changed more than 20 turns ago has aged out of the in-memory history and
/// cannot be reported here. Callers that show this to a user should say so
/// rather than implying the list is exhaustive.
pub fn original_states(checkpoints: &[Checkpoint]) -> Vec<OriginalState> {
    let mut seen: Vec<OriginalState> = Vec::new();
    for cp in checkpoints {
        for snap in &cp.files {
            if seen.iter().any(|s| s.path == snap.path) {
                continue;
            }
            seen.push(OriginalState {
                path: snap.path.clone(),
                before: snap.before.clone(),
            });
        }
    }
    seen.sort_by(|a, b| a.path.cmp(&b.path));
    seen
}

/// What actually happened when a checkpoint (or several, for `/undo n`)
/// was restored -- for the REPL to report back to the user.
pub struct UndoReport {
    pub label: String,
    pub turns_undone: usize,
    pub files_restored: usize,
    pub files_removed: usize,
    pub messages_truncated_to: usize,
}

/// Pops and restores the most recent `n` checkpoints (clamped to however
/// many actually exist). `messages` is truncated in place; files are
/// written back to disk directly.
///
/// Implemented as a straightforward repeated single-pop, not a batch/merge
/// pass over all `n` at once -- and that's provably equivalent to the
/// "correct" composed result: each successive truncate can only shrink
/// `messages` further, so after `n` pops its length is exactly the oldest
/// of the `n` checkpoints' `message_len_before`; each write to a given
/// path is overwritten by every subsequent (older) pop that also touched
/// it, so after `n` pops every file sits at its state from the *oldest*
/// checkpoint among the `n` that touched it. No path -> snapshot merge map
/// needed: repeated overwrite already converges to the last (oldest)
/// writer.
pub async fn undo(
    checkpoints: &mut Vec<Checkpoint>,
    messages: &mut Vec<Message>,
    n: usize,
) -> Option<UndoReport> {
    if checkpoints.is_empty() {
        return None;
    }

    let mut label = None;
    let mut files_restored = 0;
    let mut files_removed = 0;
    let mut turns_undone = 0;

    for _ in 0..n {
        let Some(checkpoint) = checkpoints.pop() else {
            break;
        };
        if label.is_none() {
            label = Some(checkpoint.label.clone());
        }
        for file in checkpoint.files {
            match file.before {
                Some(content) => {
                    if tokio::fs::write(&file.path, content).await.is_ok() {
                        files_restored += 1;
                    }
                }
                None => {
                    if tokio::fs::remove_file(&file.path).await.is_ok() {
                        files_removed += 1;
                    }
                }
            }
        }
        messages.truncate(checkpoint.message_len_before);
        turns_undone += 1;
    }

    Some(UndoReport {
        label: label.unwrap_or_default(),
        turns_undone,
        files_restored,
        files_removed,
        messages_truncated_to: messages.len(),
    })
}

/// Pushes a completed checkpoint, evicting the oldest if over
/// [`MAX_CHECKPOINTS`].
pub fn push(checkpoints: &mut Vec<Checkpoint>, checkpoint: Checkpoint) {
    checkpoints.push(checkpoint);
    if checkpoints.len() > MAX_CHECKPOINTS {
        checkpoints.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::value::RawValue;

    fn ws() -> Workspace {
        // A counter, not a timestamp: tests run concurrently as separate
        // threads in one process, and a nanosecond-resolution timestamp is
        // not actually a safe uniqueness guarantee under thread scheduling
        // jitter on every host -- confirmed flaky here, two tests collided
        // on the same directory and each saw the other's file content.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hivemind_checkpoint_test_{}_{n}",
            std::process::id(),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    fn call(name: &str, json: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            args: RawValue::from_string(json.to_string()).unwrap(),
        }
    }

    #[tokio::test]
    async fn capture_ignores_non_mutating_tools() {
        let w = ws();
        std::fs::write(w.root.join("a.txt"), "hi").unwrap();
        let mut cp = Checkpoint::open("do nothing", 0);
        cp.capture(
            &w,
            &[
                call("read_file", serde_json::json!({"path": "a.txt"})),
                call("list_dir", serde_json::json!({"path": "."})),
                call("run_shell", serde_json::json!({"command": "rm a.txt"})),
            ],
        )
        .await;
        assert!(cp.files.is_empty());
    }

    #[tokio::test]
    async fn capture_records_existing_file_content() {
        let w = ws();
        std::fs::write(w.root.join("a.txt"), "original").unwrap();
        let mut cp = Checkpoint::open("edit a", 3);
        cp.capture(
            &w,
            &[call(
                "edit_file",
                serde_json::json!({"path": "a.txt", "old_string": "original", "new_string": "changed"}),
            )],
        )
        .await;
        assert_eq!(cp.files.len(), 1);
        assert_eq!(cp.files[0].before.as_deref(), Some("original"));
    }

    #[tokio::test]
    async fn capture_records_absence_for_new_file() {
        let w = ws();
        let mut cp = Checkpoint::open("create b", 0);
        cp.capture(
            &w,
            &[call(
                "write_file",
                serde_json::json!({"path": "b.txt", "content": "new"}),
            )],
        )
        .await;
        assert_eq!(cp.files.len(), 1);
        assert_eq!(cp.files[0].before, None);
    }

    #[tokio::test]
    async fn capture_is_first_touch_wins_within_one_checkpoint() {
        let w = ws();
        std::fs::write(w.root.join("a.txt"), "v1").unwrap();
        let mut cp = Checkpoint::open("edit a twice", 0);
        cp.capture(
            &w,
            &[call(
                "edit_file",
                serde_json::json!({"path": "a.txt", "old_string": "v1", "new_string": "v2"}),
            )],
        )
        .await;
        // Simulate the edit having actually happened between the two calls.
        std::fs::write(w.root.join("a.txt"), "v2").unwrap();
        cp.capture(
            &w,
            &[call(
                "edit_file",
                serde_json::json!({"path": "a.txt", "old_string": "v2", "new_string": "v3"}),
            )],
        )
        .await;
        assert_eq!(cp.files.len(), 1, "same path must not be captured twice");
        assert_eq!(cp.files[0].before.as_deref(), Some("v1"));
    }

    #[tokio::test]
    async fn restoring_an_existing_file_puts_its_content_back() {
        let w = ws();
        std::fs::write(w.root.join("a.txt"), "original").unwrap();
        let mut cp = Checkpoint::open("edit a", 0);
        cp.capture(
            &w,
            &[call(
                "edit_file",
                serde_json::json!({"path": "a.txt", "old_string": "original", "new_string": "changed"}),
            )],
        )
        .await;
        std::fs::write(w.root.join("a.txt"), "changed").unwrap();

        let mut checkpoints = vec![cp];
        let mut messages = vec![Message::system("sys"), Message::user("edit a")];
        let report = undo(&mut checkpoints, &mut messages, 1).await.unwrap();

        assert_eq!(report.files_restored, 1);
        assert_eq!(report.files_removed, 0);
        assert_eq!(
            std::fs::read_to_string(w.root.join("a.txt")).unwrap(),
            "original"
        );
        assert!(checkpoints.is_empty());
    }

    #[tokio::test]
    async fn restoring_a_newly_created_file_deletes_it() {
        let w = ws();
        let mut cp = Checkpoint::open("create b", 0);
        cp.capture(
            &w,
            &[call(
                "write_file",
                serde_json::json!({"path": "b.txt", "content": "new"}),
            )],
        )
        .await;
        std::fs::write(w.root.join("b.txt"), "new").unwrap();

        let mut checkpoints = vec![cp];
        let mut messages = vec![Message::system("sys")];
        let report = undo(&mut checkpoints, &mut messages, 1).await.unwrap();

        assert_eq!(report.files_removed, 1);
        assert!(!w.root.join("b.txt").exists());
    }

    #[tokio::test]
    async fn messages_truncate_to_the_recorded_length() {
        let mut checkpoints = vec![Checkpoint::open("say hi", 1)];
        let mut messages = vec![
            Message::system("sys"),
            Message::user("say hi"),
            Message::assistant("hello!".to_string()),
        ];
        let report = undo(&mut checkpoints, &mut messages, 1).await.unwrap();
        assert_eq!(report.messages_truncated_to, 1);
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn undo_n_composes_to_the_oldest_state_not_the_intermediate_one() {
        let w = ws();
        std::fs::write(w.root.join("a.txt"), "v1").unwrap();

        let mut turn1 = Checkpoint::open("turn 1", 1);
        turn1
            .capture(
                &w,
                &[call(
                    "edit_file",
                    serde_json::json!({"path": "a.txt", "old_string": "v1", "new_string": "v2"}),
                )],
            )
            .await;
        std::fs::write(w.root.join("a.txt"), "v2").unwrap();

        let mut turn2 = Checkpoint::open("turn 2", 2);
        turn2
            .capture(
                &w,
                &[call(
                    "edit_file",
                    serde_json::json!({"path": "a.txt", "old_string": "v2", "new_string": "v3"}),
                )],
            )
            .await;
        std::fs::write(w.root.join("a.txt"), "v3").unwrap();

        let mut checkpoints = vec![turn1, turn2];
        let mut messages = vec![
            Message::system("sys"),
            Message::user("turn 1"),
            Message::user("turn 2"),
        ];

        let report = undo(&mut checkpoints, &mut messages, 2).await.unwrap();

        assert_eq!(report.turns_undone, 2);
        assert_eq!(
            std::fs::read_to_string(w.root.join("a.txt")).unwrap(),
            "v1",
            "undo(2) must land on turn 1's pre-edit state, not turn 2's"
        );
        assert_eq!(messages.len(), 1);
        assert!(checkpoints.is_empty());
    }

    #[test]
    fn push_evicts_the_oldest_past_the_cap() {
        let mut checkpoints = Vec::new();
        for i in 0..(MAX_CHECKPOINTS + 5) {
            push(&mut checkpoints, Checkpoint::open(&format!("turn {i}"), i));
        }
        assert_eq!(checkpoints.len(), MAX_CHECKPOINTS);
        assert_eq!(checkpoints.first().unwrap().label, "turn 5");
        assert_eq!(
            checkpoints.last().unwrap().label,
            format!("turn {}", MAX_CHECKPOINTS + 4)
        );
    }

    #[tokio::test]
    async fn undo_on_an_empty_stack_is_a_clean_no_op() {
        let mut checkpoints: Vec<Checkpoint> = Vec::new();
        let mut messages = vec![Message::system("sys")];
        let report = undo(&mut checkpoints, &mut messages, 1).await;
        assert!(report.is_none());
        assert_eq!(messages.len(), 1);
    }
}
