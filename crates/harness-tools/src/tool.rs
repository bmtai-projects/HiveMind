//! The unified `Tool` trait and its registry — analogue of grok-build's
//! `xai-tool-runtime` `Tool` trait + `ToolBridge`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::value::RawValue;

use harness_types::{ToolCall, ToolSchema};

use crate::error::ToolError;

/// How a tool call actually ended, as reported by the tool that ran it.
///
/// The point is that this is *reported*, never inferred. The harness used to
/// decide by string-matching the result text (`looks_like_failure`), which
/// misread 4 of 11 realistic outputs in `classification_corpus` -- every one
/// of them a success read as a failure, because reading a log, a source file
/// that formats an error, or this repo's own docs puts the markers in the
/// text. That answer feeds `update_escalation`, which switches to a model
/// costing ~25x more on output, so a misread is a cost decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolStatus {
    #[default]
    Ok,
    /// The tool ran and the work failed (non-zero exit, unwritable path, a
    /// patch that didn't apply).
    Failed,
    /// Refused before doing anything -- a hook veto, a path outside the
    /// workspace. Distinct from `Failed` because retrying is pointless.
    Denied,
    /// Exceeded its time budget. Distinct from `Failed` because the work may
    /// have partially happened and retrying may still be reasonable.
    Timeout,
}

impl ToolStatus {
    pub fn is_failure(self) -> bool {
        !matches!(self, ToolStatus::Ok)
    }
}

/// A file a tool created, modified, or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
}

/// What a tool call produced.
///
/// Deliberately split into what the *model* reads (`summary`) and what the
/// *harness* acts on (everything else). The wire format is unchanged: only
/// `summary` ever reaches a `Message`, so the NDJSON protocol, the session
/// files, and the VS Code extension all carry on as before. What changes is
/// that the harness stops parsing prose to find out what happened.
#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    pub status: ToolStatus,
    /// The text fed back to the model. For a failure this should still read
    /// as a useful error -- `status` is for the harness, not a substitute
    /// for telling the model what went wrong.
    pub summary: String,
    /// Whether the same call could plausibly succeed if repeated. `false`
    /// for a denial or a deterministic error; `true` for a timeout or a
    /// transient network failure.
    pub retryable: bool,
    /// Files this call touched. Lets the harness checkpoint and invalidate
    /// read-set entries from a reported fact rather than by re-deriving it
    /// from arguments.
    pub changed_files: Vec<FileChange>,
    /// True when `summary` is not the whole output.
    pub truncated: bool,
    /// The complete output, when the tool had to clamp what it put in
    /// `summary` to keep it sane.
    ///
    /// Exists because the artifact layer runs *after* the tool returns, so
    /// anything a tool discards internally is gone before it can ever be
    /// archived. `run_shell` caps each stream at 60 KB, which is precisely
    /// the case artifacts are for -- a 40,000-line test run whose failures
    /// sit in the elided middle. Without this channel the artifact would
    /// faithfully preserve the same truncated text and save nothing.
    ///
    /// `None` means `summary` is already whole; no tool is obliged to set
    /// this, and leaving it unset is the correct default.
    pub full_output: Option<String>,
    pub duration_ms: u64,
    /// Exact charge reported by a metered tool's HiveMind endpoint. Local
    /// tools leave this at zero. Kept outside `summary` so accounting never
    /// depends on parsing model-visible prose.
    pub cost_usd: f64,
}

impl ToolResult {
    /// A successful result carrying only text. The common case, and what
    /// `From<String>` produces.
    pub fn ok(summary: impl Into<String>) -> Self {
        Self {
            status: ToolStatus::Ok,
            summary: summary.into(),
            ..Default::default()
        }
    }

    pub fn failed(summary: impl Into<String>) -> Self {
        Self {
            status: ToolStatus::Failed,
            summary: summary.into(),
            ..Default::default()
        }
    }

    pub fn with_status(mut self, status: ToolStatus) -> Self {
        self.status = status;
        self
    }

    pub fn with_changed_file(mut self, path: impl Into<String>, kind: FileChangeKind) -> Self {
        self.changed_files.push(FileChange {
            path: path.into(),
            kind,
        });
        self
    }

    pub fn truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }
}

/// Lets a tool that hasn't been migrated yet keep returning a plain
/// `String`. Every such result is `Ok` -- which is exactly right for the
/// read-only tools, and is why `bash` (the only tool that encodes failure in
/// its text) is migrated first.
impl From<String> for ToolResult {
    fn from(summary: String) -> Self {
        Self::ok(summary)
    }
}

/// A single capability the model can invoke.
// async_trait's expansion already returns a must_use boxed future; newer
// clippy flags the macro's own must_use as redundant. Nothing here to fix.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema object describing the arguments.
    fn schema(&self) -> serde_json::Value;
    /// Run the tool. `ToolResult::summary` is fed back to the model; the rest
    /// of the envelope is for the harness. An `Err` is surfaced to the model
    /// as an error result — the agent loop keeps going; tool failure is never
    /// fatal.
    ///
    /// A tool that has nothing structured to report can return a plain
    /// `String` via `.into()`, which means `ToolStatus::Ok`.
    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError>;

    /// Optional key naming the resource *this specific call* mutates. Two
    /// calls in one batch that report the same key are run sequentially,
    /// in arrival order, instead of concurrently — see
    /// [`Registry::dispatch_many`].
    ///
    /// The default `None` means "safe to run alongside anything", which is
    /// correct for every read-only tool. Tools that write a file override
    /// it with that file's canonical path (see
    /// `crate::fs::path_conflict_key`).
    fn conflict_key(&self, _args: &RawValue) -> Option<String> {
        None
    }
}

/// Registered tools, keyed by name in a `BTreeMap` so [`Registry::schemas`]
/// is name-sorted — a deliberately stable order, since the tool manifest
/// sits in the prompt prefix that providers' context caches key on.
/// Reordering tools between turns would silently break the cache hit rate.
#[derive(Clone, Default)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    disabled: BTreeSet<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Register a capability without advertising or dispatching it yet.
    /// This is used for opt-in paid tools: toggling only changes this set,
    /// while the underlying sorted registry and schemas stay deterministic.
    pub fn register_disabled(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name.clone(), tool);
        self.disabled.insert(name);
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        if !self.tools.contains_key(name) {
            return false;
        }
        if enabled {
            self.disabled.remove(name);
        } else {
            self.disabled.insert(name.to_string());
        }
        true
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        self.tools.contains_key(name) && !self.disabled.contains(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools
            .keys()
            .filter(|name| !self.disabled.contains(*name))
            .map(String::as_str)
            .collect()
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .filter(|(name, _)| !self.disabled.contains(*name))
            .map(|(_, t)| t)
            .map(|t| ToolSchema {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.schema(),
            })
            .collect()
    }

    /// Execute every call in `calls`, then return results in the **same
    /// order the calls arrived in** — concurrency changes completion
    /// timing, not the message sequence the model sees next. Stable
    /// ordering matters twice over: it keeps the transcript deterministic
    /// for the user, and it keeps the resulting prefix consistent
    /// turn-to-turn for prompt caching.
    ///
    /// Calls run concurrently *except* where two of them report the same
    /// [`Tool::conflict_key`] — those run sequentially, in arrival order.
    /// Without that, a turn batching two `edit_file`s on one file would
    /// have both read the same original, both compute a replacement from
    /// it, and the second write would silently discard the first (each
    /// `edit_file` is a read-modify-write; see `crate::edit`).
    ///
    /// Every call is guaranteed exactly one result, even if its tool
    /// panicked: the returned `Vec` always has one entry per input call,
    /// in order.
    pub async fn dispatch_many(&self, calls: Vec<ToolCall>) -> Vec<(ToolCall, ToolResult)> {
        /// One call awaiting dispatch: its original position (so results
        /// can be restored to arrival order), the call, and the resolved
        /// tool (`None` for a name the registry doesn't know).
        type PendingCall = (usize, ToolCall, Option<Arc<dyn Tool>>);

        // Bucket conflicting calls together, preserving arrival order both
        // across buckets and within each one.
        let mut groups: Vec<Vec<PendingCall>> = Vec::new();
        let mut group_of_key: HashMap<String, usize> = HashMap::new();
        // Kept out here so a call whose task dies can still be answered
        // below -- the spawned task owns the `ToolCall` itself.
        let mut identities: Vec<(String, String)> = Vec::new();

        for (idx, call) in calls.into_iter().enumerate() {
            identities.push((call.id.clone(), call.name.clone()));
            let tool = self
                .tools
                .get(&call.name)
                .filter(|_| !self.disabled.contains(&call.name))
                .cloned();
            match tool.as_ref().and_then(|t| t.conflict_key(&call.args)) {
                Some(key) => match group_of_key.get(&key) {
                    Some(&g) => groups[g].push((idx, call, tool)),
                    None => {
                        group_of_key.insert(key, groups.len());
                        groups.push(vec![(idx, call, tool)]);
                    }
                },
                None => groups.push(vec![(idx, call, tool)]),
            }
        }

        let mut set = tokio::task::JoinSet::new();
        for group in groups {
            set.spawn(async move {
                let mut finished = Vec::with_capacity(group.len());
                for (idx, call, tool) in group {
                    // A tool that returns Err never got to report a status
                    // itself, so the dispatcher assigns one. `ERROR:` stays
                    // on the summary because that is what the model has
                    // always read; what is new is that the harness now
                    // learns the same fact from `status` instead of from
                    // that prefix.
                    let result = match tool {
                        Some(t) => match t.execute(&call.args).await {
                            Ok(r) => r,
                            Err(e) => ToolResult::failed(format!("ERROR: {e}")),
                        },
                        None => {
                            ToolResult::failed(format!("ERROR: unknown tool \"{}\"", call.name))
                        }
                    };
                    finished.push((idx, call, result));
                }
                finished
            });
        }

        let mut slots: Vec<Option<(ToolCall, ToolResult)>> = std::iter::repeat_with(|| None)
            .take(identities.len())
            .collect();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(finished) => {
                    for (idx, call, result) in finished {
                        slots[idx] = Some((call, result));
                    }
                }
                Err(join_err) => {
                    // A tool panicked, taking its whole bucket's results
                    // with it. Left unfilled here and reconciled below.
                    tracing_stub(&format!("tool task panicked: {join_err}"));
                }
            }
        }

        // Backfill anything a panic left empty. Dropping a slot instead
        // would desynchronize the transcript: the assistant message
        // already carries that `tool_call`, and an OpenAI-dialect API
        // rejects the *next* request outright when a tool_call has no
        // matching tool result. A synthetic error keeps the pairing valid
        // and lets the model see what happened and recover.
        slots
            .into_iter()
            .enumerate()
            .map(|(idx, slot)| {
                slot.unwrap_or_else(|| {
                    let (id, name) = identities[idx].clone();
                    let result = ToolResult::failed(format!(
                        "ERROR: tool \"{name}\" panicked and returned no result"
                    ));
                    let call = ToolCall {
                        id,
                        name,
                        args: RawValue::from_string("{}".to_string())
                            .expect("literal is valid JSON"),
                    };
                    (call, result)
                })
            })
            .collect()
    }
}

/// No logging framework dependency in this crate; stderr is enough for a
/// panic that should never happen in practice.
fn tracing_stub(msg: &str) {
    eprintln!("[harness-tools] {msg}");
}

/// Guess whether a tool result represents a failure, by reading its text.
///
/// **Superseded by [`ToolStatus`], and retained only as the baseline that
/// test proves an improvement against.** Tools now report how they ended,
/// so nothing in the harness needs to infer it.
///
/// It is kept rather than deleted because deleting it would delete the
/// evidence. `string_matching_misreads_ordinary_successes` scores this
/// function on [`classification_corpus`] and
/// `a_reported_status_cannot_be_misread` scores the replacement on the same
/// eleven cases; the pair is what stops the old approach quietly returning.
///
/// Why it could never work: the markers it looks for are ordinary text. A
/// log whose first line is `ERROR:`, a source file that formats one, this
/// repo's own docs describing the `[exit: N]` convention -- all read as
/// failed calls. And that answer fed `update_escalation`, which switches to
/// a model costing ~25x more on output.
#[cfg(test)]
fn looks_like_failure(result: &str) -> bool {
    if result.starts_with("ERROR:") {
        return true;
    }
    // Markers are emitted at the start of their own line (see
    // `bash::format_output`), so anchor there -- a file whose *contents*
    // mention "[exit:" is not a failure.
    result
        .lines()
        .any(|l| l.starts_with("[exit:") || l.starts_with("[timed out after"))
}

/// Labeled tool outputs, used to measure how well the harness can tell a
/// failed tool call from a successful one.
///
/// Every entry is a shape a real tool actually produces. `true` means the
/// call genuinely failed. This exists because the harness's answer to that
/// question feeds `update_escalation`, which switches to a model costing
/// ~25x more on output -- so a misread here is a cost decision, not a
/// cosmetic one.
#[cfg(test)]
pub(crate) fn classification_corpus() -> Vec<(&'static str, &'static str, bool)> {
    vec![
        // --- genuine failures -------------------------------------------
        ("tool error", "ERROR: no such file", true),
        (
            "shell non-zero exit",
            "bind EADDRINUSE 0.0.0.0:3001\n[exit: exit status: 1]",
            true,
        ),
        (
            "shell timeout",
            "[timed out after 120s; the command and anything it started were killed.]",
            true,
        ),
        // --- genuine successes ------------------------------------------
        ("plain output", "hello\n", false),
        ("empty run", "(no output; exit 0)", false),
        ("json result", "{\"ok\":true}", false),
        (
            "marker mentioned mid-line",
            "the docs say results end with [exit: status] on failure",
            false,
        ),
        // The cases the line-anchored heuristic gets wrong. Each is an
        // ordinary `read_file` or `search` on this very repo.
        (
            "read_file of a log whose first line is an error",
            "ERROR: connection refused\nERROR: retrying\n",
            false,
        ),
        (
            "read_file of code that formats an error",
            "ERROR: {msg}\", e);\n    Ok(())\n",
            false,
        ),
        (
            "read_file of bash.rs documenting its own marker",
            "[exit: N] is printed when a command fails.\n",
            false,
        ),
        (
            "search hit quoting the timeout marker at line start",
            "[timed out after 30s] appears in bash.rs\n",
            false,
        ),
    ]
}

/// Small JSON-Schema object builder so tool definitions stay readable.
pub fn obj_schema(props: &[(&str, serde_json::Value)], required: &[&str]) -> serde_json::Value {
    let properties: serde_json::Map<String, serde_json::Value> = props
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::edit::EditFile;
    use crate::fs::{Workspace, WriteFile};
    use std::sync::Mutex;
    use std::time::Duration;

    fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            args: RawValue::from_string(args.to_string()).unwrap(),
        }
    }

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!(
            "hivemind_dispatch_test_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    /// Records the order in which its executions start and finish, so a test
    /// can tell genuine concurrency from serialization.
    struct Recorder {
        log: Arc<Mutex<Vec<String>>>,
        key: Option<String>,
    }

    #[async_trait]
    impl Tool for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn schema(&self) -> serde_json::Value {
            obj_schema(&[], &[])
        }
        fn conflict_key(&self, _args: &RawValue) -> Option<String> {
            self.key.clone()
        }
        async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
            let tag = args.get().to_string();
            self.log.lock().unwrap().push(format!("start {tag}"));
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.log.lock().unwrap().push(format!("end {tag}"));
            Ok(ToolResult::ok(tag))
        }
    }

    struct Panicker;

    #[async_trait]
    impl Tool for Panicker {
        fn name(&self) -> &str {
            "panicker"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn schema(&self) -> serde_json::Value {
            obj_schema(&[], &[])
        }
        async fn execute(&self, _args: &RawValue) -> Result<ToolResult, ToolError> {
            panic!("boom");
        }
    }

    #[tokio::test]
    async fn calls_without_a_conflict_key_still_run_concurrently() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut r = Registry::new();
        r.register(Arc::new(Recorder {
            log: log.clone(),
            key: None,
        }));

        let out = r
            .dispatch_many(vec![
                call("a", "recorder", serde_json::json!({ "n": 1 })),
                call("b", "recorder", serde_json::json!({ "n": 2 })),
            ])
            .await;

        assert_eq!(out.len(), 2);
        // Both start before either finishes -- that's the whole point of
        // parallel dispatch, and this is what makes batching a real speedup.
        let log = log.lock().unwrap().clone();
        assert!(
            log[0].starts_with("start") && log[1].starts_with("start"),
            "expected interleaved concurrent execution, got {log:?}"
        );
    }

    #[tokio::test]
    async fn calls_sharing_a_conflict_key_are_serialized_in_arrival_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut r = Registry::new();
        r.register(Arc::new(Recorder {
            log: log.clone(),
            key: Some("same-file".into()),
        }));

        r.dispatch_many(vec![
            call("a", "recorder", serde_json::json!({ "n": 1 })),
            call("b", "recorder", serde_json::json!({ "n": 2 })),
        ])
        .await;

        // Strictly start/end/start/end: the second never begins until the
        // first has fully finished writing.
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "start {\"n\":1}",
                "end {\"n\":1}",
                "start {\"n\":2}",
                "end {\"n\":2}"
            ],
            "same-key calls must not overlap"
        );
    }

    /// The real-world reason `conflict_key` exists: two `edit_file` calls on
    /// one file in a single batched turn. Both read the original; without
    /// serialization the later write clobbers the earlier one.
    #[tokio::test]
    async fn two_edits_to_one_file_in_a_batch_both_survive() {
        let w = ws("two_edits");
        std::fs::write(w.root.join("app.js"), "const A = 1;\nconst B = 2;\n").unwrap();

        let mut r = Registry::new();
        r.register(Arc::new(EditFile(w.clone())));

        let out = r
            .dispatch_many(vec![
                call(
                    "1",
                    "edit_file",
                    serde_json::json!({"path": "app.js", "old_string": "const A = 1;", "new_string": "const A = 100;"}),
                ),
                call(
                    "2",
                    "edit_file",
                    serde_json::json!({"path": "app.js", "old_string": "const B = 2;", "new_string": "const B = 200;"}),
                ),
            ])
            .await;

        assert_eq!(out.len(), 2);
        assert!(
            !out[0].1.status.is_failure(),
            "first edit: {}",
            out[0].1.summary
        );
        assert!(
            !out[1].1.status.is_failure(),
            "second edit: {}",
            out[1].1.summary
        );

        let final_text = std::fs::read_to_string(w.root.join("app.js")).unwrap();
        assert!(
            final_text.contains("const A = 100;"),
            "lost the first edit: {final_text}"
        );
        assert!(
            final_text.contains("const B = 200;"),
            "lost the second edit: {final_text}"
        );
    }

    #[tokio::test]
    async fn the_same_file_named_two_different_ways_still_collides() {
        // "./app.js" and "app.js" resolve to one canonical path, so they
        // must land in the same serialization bucket -- keying on the raw
        // argument string would let these race.
        let w = ws("path_aliases");
        std::fs::write(w.root.join("app.js"), "x = 1;\ny = 2;\n").unwrap();

        let mut r = Registry::new();
        r.register(Arc::new(EditFile(w.clone())));

        r.dispatch_many(vec![
            call(
                "1",
                "edit_file",
                serde_json::json!({"path": "app.js", "old_string": "x = 1;", "new_string": "x = 11;"}),
            ),
            call(
                "2",
                "edit_file",
                serde_json::json!({"path": "./app.js", "old_string": "y = 2;", "new_string": "y = 22;"}),
            ),
        ])
        .await;

        let final_text = std::fs::read_to_string(w.root.join("app.js")).unwrap();
        assert!(final_text.contains("x = 11;"), "lost an edit: {final_text}");
        assert!(final_text.contains("y = 22;"), "lost an edit: {final_text}");
    }

    #[tokio::test]
    async fn writes_to_different_files_are_not_serialized() {
        // The batching win itself: independent files must stay concurrent.
        let w = ws("different_files");
        let mut r = Registry::new();
        r.register(Arc::new(WriteFile(w.clone())));

        let out = r
            .dispatch_many(vec![
                call(
                    "1",
                    "write_file",
                    serde_json::json!({"path": "a.txt", "content": "A"}),
                ),
                call(
                    "2",
                    "write_file",
                    serde_json::json!({"path": "b.txt", "content": "B"}),
                ),
                call(
                    "3",
                    "write_file",
                    serde_json::json!({"path": "c.txt", "content": "C"}),
                ),
            ])
            .await;

        assert_eq!(out.len(), 3);
        assert_eq!(std::fs::read_to_string(w.root.join("a.txt")).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(w.root.join("b.txt")).unwrap(), "B");
        assert_eq!(std::fs::read_to_string(w.root.join("c.txt")).unwrap(), "C");
    }

    #[tokio::test]
    async fn a_panicking_tool_still_produces_a_result_for_its_call() {
        // Dropping the slot would leave the assistant's tool_call unpaired,
        // which the *next* API request rejects outright.
        let mut r = Registry::new();
        r.register(Arc::new(Panicker));

        let out = r
            .dispatch_many(vec![
                call("a", "panicker", serde_json::json!({})),
                call("b", "panicker", serde_json::json!({})),
            ])
            .await;

        assert_eq!(out.len(), 2, "every call must come back with a result");
        assert_eq!(out[0].0.id, "a");
        assert_eq!(out[1].0.id, "b");
        assert!(out.iter().all(|(_, res)| res.status.is_failure()));
    }

    #[tokio::test]
    async fn an_unknown_tool_reports_an_error_without_disturbing_its_neighbors() {
        let w = ws("unknown_tool");
        let mut r = Registry::new();
        r.register(Arc::new(WriteFile(w.clone())));

        let out = r
            .dispatch_many(vec![
                call("1", "no_such_tool", serde_json::json!({})),
                call(
                    "2",
                    "write_file",
                    serde_json::json!({"path": "ok.txt", "content": "fine"}),
                ),
            ])
            .await;

        assert_eq!(out.len(), 2);
        assert!(out[0].1.summary.contains("unknown tool"));
        assert!(!out[1].1.status.is_failure());
        assert_eq!(
            std::fs::read_to_string(w.root.join("ok.txt")).unwrap(),
            "fine"
        );
    }

    #[tokio::test]
    async fn a_disabled_tool_is_absent_from_manifest_and_cannot_dispatch() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut r = Registry::new();
        r.register_disabled(Arc::new(Recorder {
            log: log.clone(),
            key: None,
        }));

        assert!(
            r.contains("recorder"),
            "the hosted capability stays registered"
        );
        assert!(!r.is_enabled("recorder"));
        assert!(r.names().is_empty());
        assert!(r.schemas().is_empty());

        let denied = r
            .dispatch_many(vec![call("off", "recorder", serde_json::json!({}))])
            .await;
        assert!(denied[0].1.status.is_failure());
        assert!(denied[0].1.summary.contains("unknown tool"));
        assert!(
            log.lock().unwrap().is_empty(),
            "disabled means it did not run"
        );

        assert!(r.set_enabled("recorder", true));
        assert!(r.is_enabled("recorder"));
        assert_eq!(r.names(), vec!["recorder"]);
        assert_eq!(r.schemas().len(), 1);

        let allowed = r
            .dispatch_many(vec![call("on", "recorder", serde_json::json!({}))])
            .await;
        assert!(!allowed[0].1.status.is_failure());
        assert!(
            !log.lock().unwrap().is_empty(),
            "enabling makes it dispatchable"
        );

        assert!(r.set_enabled("recorder", false));
        assert!(r.names().is_empty());
        assert!(!r.set_enabled("missing", true));
    }

    #[tokio::test]
    async fn results_keep_arrival_order_regardless_of_completion_order() {
        struct SlowFirst;
        #[async_trait]
        impl Tool for SlowFirst {
            fn name(&self) -> &str {
                "slow"
            }
            fn description(&self) -> &str {
                "test"
            }
            fn schema(&self) -> serde_json::Value {
                obj_schema(&[], &[])
            }
            async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
                #[derive(serde::Deserialize)]
                struct A {
                    ms: u64,
                }
                let a: A = serde_json::from_str(args.get()).unwrap();
                tokio::time::sleep(Duration::from_millis(a.ms)).await;
                Ok(ToolResult::ok(format!("slept {}", a.ms)))
            }
        }

        let mut r = Registry::new();
        r.register(Arc::new(SlowFirst));

        // The first call finishes last; the returned order must still be
        // the order the model asked for.
        let out = r
            .dispatch_many(vec![
                call("a", "slow", serde_json::json!({ "ms": 60 })),
                call("b", "slow", serde_json::json!({ "ms": 1 })),
            ])
            .await;

        assert_eq!(out[0].0.id, "a");
        assert_eq!(out[1].0.id, "b");
    }
}

/// V3 from the M1 plan: the envelope must not leak onto the wire.
///
/// The whole migration rests on hosts being unaffected -- the NDJSON
/// protocol, the saved session files, and the VS Code extension all carry
/// tool results as plain text. An integration diff of two binaries would
/// drift; this pins the invariant at the point it could actually break,
/// which is the one line in `dispatch_and_record` that maps a `ToolResult`
/// into a `Message`.
#[cfg(test)]
mod wire_compatibility_tests {
    use super::{FileChangeKind, ToolResult, ToolStatus};
    use harness_types::Message;

    #[test]
    fn only_the_summary_reaches_the_transcript() {
        // A maximally-populated envelope: if any of this could leak into a
        // Message, it would show up here.
        let rich = ToolResult {
            status: ToolStatus::Failed,
            summary: "[exit: exit status: 1]".to_string(),
            retryable: true,
            duration_ms: 1234,
            truncated: true,
            full_output: Some("the whole 2 MB of it".to_string()),
            cost_usd: 0.125,
            ..Default::default()
        }
        .with_changed_file("src/main.rs", FileChangeKind::Modified);

        // Exactly what the agent does with it.
        let msg = Message::tool_result("call-1", "run_shell", rich.summary.clone());

        assert_eq!(msg.content, "[exit: exit status: 1]");

        let json = serde_json::to_value(&msg).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for leaked in [
            "status",
            "retryable",
            "duration_ms",
            "truncated",
            "full_output",
            "changed_files",
            "cost_usd",
        ] {
            assert!(
                !keys.contains(&leaked),
                "`{leaked}` reached the wire; hosts would need a coordinated release: {keys:?}"
            );
        }
    }

    #[test]
    fn a_failing_status_still_tells_the_model_what_happened() {
        // `status` is for the harness, never a substitute for saying so in
        // the text -- the model only ever reads `summary`.
        let r = ToolResult::failed("ERROR: no such file".to_string());
        assert!(r.status.is_failure());
        assert!(
            !r.summary.is_empty(),
            "a status the model cannot see is not an error message"
        );
    }
}

#[cfg(test)]
mod failure_detection_tests {
    use super::looks_like_failure;

    #[test]
    fn a_tool_level_error_is_a_failure() {
        assert!(looks_like_failure("ERROR: no such file"));
    }

    /// The case the agent's stall detection used to miss entirely: a shell
    /// command that ran fine as a *tool call* but failed as a command.
    #[test]
    fn a_non_zero_shell_exit_is_a_failure() {
        assert!(looks_like_failure(
            "bind EADDRINUSE 0.0.0.0:3001\n[exit: exit status: 1]"
        ));
        assert!(looks_like_failure("[exit: exit status: 7]"));
    }

    #[test]
    fn a_timed_out_command_is_a_failure() {
        assert!(looks_like_failure(
            "[timed out after 120s; the command and anything it started were killed.]"
        ));
    }

    #[test]
    fn ordinary_successful_output_is_not_a_failure() {
        assert!(!looks_like_failure("hello\n"));
        assert!(!looks_like_failure("(no output; exit 0)"));
        assert!(!looks_like_failure("{\"ok\":true}"));
    }

    /// The measured starting point, kept as a test rather than a claim in a
    /// commit message. `looks_like_failure` decides by string-matching, so
    /// any tool output that *contains* the markers -- source, logs, docs,
    /// search hits -- reads as a failed call.
    ///
    /// Retained after M1 so the improvement stays visible and cannot quietly
    /// regress: this is the old approach's score, and
    /// `a_reported_status_cannot_be_misread` is the new one's on the same
    /// corpus.
    #[test]
    fn string_matching_misreads_ordinary_successes() {
        let corpus = super::classification_corpus();
        let wrong: Vec<&str> = corpus
            .iter()
            .filter(|(_, body, is_failure)| looks_like_failure(body) != *is_failure)
            .map(|(what, _, _)| *what)
            .collect();

        assert_eq!(
            wrong.len(),
            4,
            "baseline changed -- update the count and say why: {wrong:?}"
        );
        // All four are successes read as failures, which is the expensive
        // direction: it drives the escalation counter.
        for what in &wrong {
            let (_, body, _) = corpus.iter().find(|(w, _, _)| w == what).unwrap();
            assert!(
                looks_like_failure(body),
                "{what} should be a false positive"
            );
        }
    }

    /// The same corpus, decided by the tool instead of by the reader.
    ///
    /// This is the whole of M1 in one assertion. `looks_like_failure` scores
    /// 4 wrong on these eleven; a reported `ToolStatus` scores zero, and not
    /// because the matching got cleverer -- there is no matching. The tool
    /// that ran the command knows its exit code and says so, and no amount
    /// of `ERROR:` inside a file it happened to read can change that.
    #[test]
    fn a_reported_status_cannot_be_misread() {
        use super::ToolResult;

        let wrong = super::classification_corpus()
            .into_iter()
            .filter(|(_, body, is_failure)| {
                // What the migrated tools now build: the status is set from
                // the fact (an exit code, a denial), and the same text rides
                // along as the summary for the model to read.
                let reported = if *is_failure {
                    ToolResult::failed(body.to_string())
                } else {
                    ToolResult::ok(body.to_string())
                };
                reported.status.is_failure() != *is_failure
            })
            .count();

        assert_eq!(
            wrong, 0,
            "a reported status is never inferred, so it cannot be wrong"
        );
    }

    /// The markers are line-anchored, so a file that merely talks about
    /// them isn't misread as a failed command.
    #[test]
    fn output_that_merely_mentions_a_marker_is_not_a_failure() {
        assert!(!looks_like_failure(
            "the docs say results end with [exit: status] on failure"
        ));
        assert!(!looks_like_failure("grep found: // [timed out after N]"));
    }
}
