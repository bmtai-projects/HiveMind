//! The unified `Tool` trait and its registry — analogue of grok-build's
//! `xai-tool-runtime` `Tool` trait + `ToolBridge`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::value::RawValue;

use harness_types::{ToolCall, ToolSchema};

use crate::error::ToolError;

/// A single capability the model can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema object describing the arguments.
    fn schema(&self) -> serde_json::Value;
    /// Run the tool. The returned string is fed back to the model as the
    /// tool result. An `Err` is surfaced to the model as an error result —
    /// the agent loop keeps going; tool failure is never fatal.
    async fn execute(&self, args: &RawValue) -> Result<String, ToolError>;

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
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
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
    pub async fn dispatch_many(&self, calls: Vec<ToolCall>) -> Vec<(ToolCall, String)> {
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
            let tool = self.tools.get(&call.name).cloned();
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
                    let result = match tool {
                        Some(t) => match t.execute(&call.args).await {
                            Ok(s) => s,
                            Err(e) => format!("ERROR: {e}"),
                        },
                        None => format!("ERROR: unknown tool \"{}\"", call.name),
                    };
                    finished.push((idx, call, result));
                }
                finished
            });
        }

        let mut slots: Vec<Option<(ToolCall, String)>> = std::iter::repeat_with(|| None)
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
                    let result = format!("ERROR: tool \"{name}\" panicked and returned no result");
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

/// Did this tool result represent a failure the model should notice?
///
/// `"ERROR:"` alone is not enough, and assuming it was is what left the
/// agent's stall detection blind to the most common kind of trouble. A
/// shell command that exits non-zero, or times out, is reported through
/// `Ok(...)` -- deliberately, because its output is still worth reading --
/// so it never started with `"ERROR:"` and never counted as anything going
/// wrong. A real session ran fourteen consecutive failing commands
/// (`pkill`, `lsof`, retry, repeat) without a single one registering.
///
/// Lives here, next to the tools whose output conventions it recognizes,
/// rather than in the agent: the agent should not have to know how
/// `run_shell` formats an exit status.
pub fn looks_like_failure(result: &str) -> bool {
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
        async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
            let tag = args.get().to_string();
            self.log.lock().unwrap().push(format!("start {tag}"));
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.log.lock().unwrap().push(format!("end {tag}"));
            Ok(tag)
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
        async fn execute(&self, _args: &RawValue) -> Result<String, ToolError> {
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
        assert!(!out[0].1.starts_with("ERROR"), "first edit: {}", out[0].1);
        assert!(!out[1].1.starts_with("ERROR"), "second edit: {}", out[1].1);

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
        assert!(out.iter().all(|(_, res)| res.starts_with("ERROR")));
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
        assert!(out[0].1.contains("unknown tool"));
        assert!(!out[1].1.starts_with("ERROR"));
        assert_eq!(
            std::fs::read_to_string(w.root.join("ok.txt")).unwrap(),
            "fine"
        );
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
            async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
                #[derive(serde::Deserialize)]
                struct A {
                    ms: u64,
                }
                let a: A = serde_json::from_str(args.get()).unwrap();
                tokio::time::sleep(Duration::from_millis(a.ms)).await;
                Ok(format!("slept {}", a.ms))
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
