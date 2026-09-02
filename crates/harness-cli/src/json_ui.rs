//! JSON-protocol implementation of [`harness_agent::Ui`]: every callback
//! writes exactly one ndjson (newline-delimited JSON) object to stdout and
//! flushes immediately — so a host process (e.g. a VS Code extension)
//! driving `hivemind` as a subprocess sees streamed deltas as they happen,
//! not batched at turn end. Selected via `hivemind activate --protocol
//! json`; see `main.rs`'s `run_json_protocol` for the stdin dispatch side.
//!
//! Every event is built with `serde_json::json!` directly against the
//! protocol spec this was implemented from, rather than through a shared
//! typed `Serialize` enum — the spec has a couple of same-`"type"`,
//! different-shape events (`undo_result`'s `nothing_to_undo` case), which a
//! single internally-tagged enum can't express cleanly. Building each
//! object inline keeps the field-name mapping obvious and auditable
//! line-by-line against the spec, which matters more here than the usual
//! benefit of a typed enum (a different process, built independently
//! against the same spec, is the other end of this wire — a silent typo
//! here breaks it silently on the other side).

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use harness_agent::Ui;
use harness_types::Usage;
use serde::Deserialize;
use serde_json::json;

/// One line of stdin in `--protocol json` mode. Field names/shapes mirror
/// the protocol spec exactly — see module docs.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    UserMessage {
        text: String,
    },
    SetModel {
        model: String,
    },
    SetReasoningEffort {
        #[serde(default)]
        effort: Option<String>,
    },
    SetBudget {
        #[serde(default)]
        budget_usd: Option<f64>,
    },
    SetWebEnabled {
        enabled: bool,
    },
    /// Select a skill by id, or clear the active one with `null`/absent.
    SetSkill {
        #[serde(default)]
        skill: Option<String>,
    },
    Undo {
        n: usize,
    },
    ForceCompact,
    Approve {
        request_id: String,
        approved: bool,
    },
    /// A message sent *while* a turn is running. Unlike `UserMessage` this
    /// is never queued behind the in-flight run -- it's handed straight to
    /// the agent's interjection queue by the reader task, for delivery at
    /// the next turn boundary.
    Interject {
        text: String,
    },
    /// Cancel the turn currently running. Like `Approve`/`Interject`, and
    /// for the same reason, this is resolved immediately by the reader task
    /// rather than queued behind `tx` -- queueing it behind the very run it
    /// exists to interrupt would mean it's only ever seen after that run
    /// finishes on its own, which is not an abort. A no-op if no turn is
    /// running (the reader task cannot know that without racing the main
    /// loop, so the main loop drops it silently if it arrives late).
    Abort,
}

pub struct JsonUi {
    /// One pending shell-approval channel per outstanding `approval_request`
    /// event, keyed by the request id embedded in that event. Populated by
    /// `request_shell_approval` — which is `Bash`'s synchronous `ApproveFn`,
    /// always invoked via `spawn_blocking` (see
    /// `harness_tools::bash::Bash::approved`), i.e. already a real OS
    /// thread, so blocking `recv()` there is safe. Resolved by the stdin
    /// loop in `main.rs::run_json_protocol` when a matching
    /// `{"type":"approve",...}` line arrives.
    pending_approvals: Mutex<HashMap<String, std::sync::mpsc::Sender<bool>>>,
    next_request_id: AtomicU64,
}

impl JsonUi {
    pub fn new() -> Self {
        Self {
            pending_approvals: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
        }
    }

    /// Write one JSON object followed by `\n` and flush immediately, atomic
    /// with respect to any other thread's concurrent `emit` (holds
    /// `Stdout`'s own lock across both the write and the flush) — matters
    /// because `Registry::dispatch_many` runs a turn's tool calls
    /// concurrently, so `tool_start`/`tool_end` can legitimately fire from
    /// several threads at once.
    fn emit(&self, value: serde_json::Value) {
        let mut line = value.to_string();
        line.push('\n');
        let mut out = io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }

    /// Emit `ready` once at startup — a structured mirror of what
    /// `print_model_catalog()` prints for a human, built from the same
    /// `harness_config::KNOWN_MODELS` table so the two can never drift.
    ///
    /// `session_id` names the file under `default_sessions_dir()` this
    /// process is writing to. Without it a host has no way to tell which
    /// saved conversation belongs to which child process, so it cannot
    /// offer to reopen one later — guessing by "newest file for this
    /// workspace" races as soon as two sessions start at once. `None` when
    /// persistence is off, which a host must treat as "this conversation
    /// will not be resumable" rather than as an error.
    pub fn emit_ready(
        &self,
        session_id: Option<&str>,
        web_available: bool,
        web_enabled: bool,
        active_skill: Option<&str>,
    ) {
        let models: Vec<_> = harness_config::KNOWN_MODELS
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "display_name": m.display_name,
                    "context_window": m.context_window,
                    "input_per_m": m.wholesale_pricing.input_per_m,
                    "output_per_m": m.wholesale_pricing.output_per_m,
                    "reasoning_efforts": m.reasoning_efforts,
                })
            })
            .collect();
        // Same source of truth `set_skill` validates against, so a host can
        // never be offered a skill the agent would then reject.
        let skills: Vec<_> = harness_agent::skills::all()
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "name": s.name,
                    "description": s.description,
                })
            })
            .collect();
        self.emit(json!({
            "type": "ready",
            "models": models,
            "session_id": session_id,
            "web_available": web_available,
            "web_enabled": web_enabled,
            "skills": skills,
            "active_skill": active_skill,
        }));
    }

    pub fn emit_web_mode(&self, available: bool, enabled: bool) {
        self.emit(json!({
            "type": "web_mode",
            "available": available,
            "enabled": enabled,
        }));
    }

    pub fn emit_skill_mode(&self, active_skill: Option<&str>) {
        self.emit(json!({
            "type": "skill_mode",
            "active_skill": active_skill,
        }));
    }

    /// Emitted once, right after `ready`, only when a `--resume`d/`--continue`d
    /// session actually has prior conversation in it (a fresh session's
    /// `history()` is exactly the one system message, which produces `[]`
    /// here and is skipped entirely rather than emitted empty).
    ///
    /// Without this, a resumed session's history exists only inside the
    /// agent's own in-memory context for its *next* model call -- nothing
    /// ever told a host what was actually said before, so the CLI resumed
    /// correctly while the UI showed a blank transcript for it.
    ///
    /// Deliberately the flat message list, not pre-grouped into UI turns:
    /// grouping (which raw `Assistant`/`Tool` messages belong to one logical
    /// reply, spanning however many internal tool round-trips the model
    /// took) is exactly what the live event stream already does, turn by
    /// turn, on the host side -- see `HiveMind-vscode/src/sessionState.ts`'s
    /// `currentAssistantTurn()`. Re-deriving the same grouping here would be
    /// a second implementation of that logic that can drift from the first.
    ///
    /// `is_error` on a historical tool call is never included: `ToolStatus`
    /// is harness-internal by design (see `ToolResult`'s own docs -- only
    /// `summary` ever reaches a `Message`) and was never persisted to disk.
    /// Guessing it back from the result text is exactly the failure mode M1
    /// replaced; a host must render historical tool results as neutral, not
    /// guess pass/fail from prose.
    pub fn emit_history(&self, messages: &[harness_types::Message]) {
        if let Some(wire) = history_wire_messages(messages) {
            self.emit(json!({"type": "history", "messages": wire}));
        }
    }

    /// Emitted after every incoming command settles — both a
    /// `user_message`'s `agent.run()` call (success or error) and every
    /// other command (`set_model`, `undo`, ...). The spec leaves the exact
    /// shape of the latter to our judgment; reusing `turn_done` verbatim
    /// rather than inventing a second "ack" shape keeps one meaning for the
    /// extension to key off: "you may send the next line now," which holds
    /// equally whether what just finished was a message or a command.
    pub fn emit_turn_done(&self) {
        self.emit(json!({"type": "turn_done"}));
    }

    /// Sent instead of (never in addition to) `turn_done` when an
    /// `abort` command actually cancelled a running turn -- so the
    /// extension can render "stopped" rather than a turn that quietly
    /// produced no new content. A `repaired` flag distinguishes a clean
    /// stop between tool calls from one that landed mid-tool-call and had
    /// to drop an incomplete call from the transcript, which is worth a
    /// different message: the second case is the one place aborting can
    /// visibly shorten what the model already did.
    pub fn emit_aborted(&self, repaired: bool) {
        self.emit(json!({"type": "aborted", "repaired": repaired}));
    }

    pub fn emit_error(&self, message: &str) {
        self.emit(json!({"type": "error", "message": message}));
    }

    pub fn emit_undo_result(&self, report: Option<&harness_agent::UndoReport>) {
        match report {
            Some(r) => self.emit(json!({
                "type": "undo_result",
                "label": r.label,
                "turns_undone": r.turns_undone,
                "files_restored": r.files_restored,
                "files_removed": r.files_removed,
            })),
            None => self.emit(json!({"type": "undo_result", "nothing_to_undo": true})),
        }
    }

    /// The synchronous `ApproveFn` handed to `Bash::with_approval` in
    /// protocol-json mode. Generates a request id, registers a channel for
    /// it, emits `approval_request`, then blocks (on a real OS thread —
    /// see the struct doc comment) until `resolve_approval` is called with
    /// a matching id, or the sender is dropped.
    pub fn request_shell_approval(&self, command: &str) -> bool {
        let request_id = format!(
            "appr-{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending_approvals
            .lock()
            .expect("pending_approvals mutex poisoned")
            .insert(request_id.clone(), tx);
        self.emit(json!({
            "type": "approval_request",
            "request_id": request_id,
            "command": command,
        }));
        // `Err` means the sender was dropped without ever answering (e.g.
        // stdin hit EOF while this was pending) -- fail closed, matching
        // the terminal UI's own default on unreadable input.
        rx.recv().unwrap_or(false)
    }

    /// Called from the stdin loop when an `{"type":"approve",...}` line
    /// arrives. An unknown or already-resolved `request_id` is silently
    /// ignored -- a duplicate or late answer from the extension isn't an
    /// error worth surfacing.
    pub fn resolve_approval(&self, request_id: &str, approved: bool) {
        if let Some(tx) = self
            .pending_approvals
            .lock()
            .expect("pending_approvals mutex poisoned")
            .remove(request_id)
        {
            let _ = tx.send(approved);
        }
    }
}

impl Default for JsonUi {
    fn default() -> Self {
        Self::new()
    }
}

impl Ui for JsonUi {
    fn turn_started(&self) {
        self.emit(json!({"type": "turn_started"}));
    }

    fn assistant_delta(&self, text: &str) {
        self.emit(json!({"type": "assistant_delta", "text": text}));
    }

    fn reasoning_delta(&self, text: &str) {
        self.emit(json!({"type": "reasoning_delta", "text": text}));
    }

    fn assistant_done(&self) {
        self.emit(json!({"type": "assistant_done"}));
    }

    fn tool_call_pending(&self, name: &str) {
        self.emit(json!({"type": "tool_call_pending", "name": name}));
    }

    fn tool_start(&self, name: &str, args: &str) {
        self.emit(json!({"type": "tool_start", "name": name, "args": args}));
    }

    fn tool_end(
        &self,
        name: &str,
        result: &str,
        is_error: bool,
        cost_usd: f64,
        session_cost_usd: f64,
    ) {
        let mut event = json!({
            "type": "tool_end",
            "name": name,
            "result": result,
            "is_error": is_error,
        });
        if cost_usd > 0.0 {
            let object = event.as_object_mut().expect("tool_end is an object");
            object.insert("cost_usd".into(), json!(cost_usd));
            object.insert("session_cost_usd".into(), json!(session_cost_usd));
        }
        self.emit(event);
    }

    fn usage(&self, usage: &Usage, model_id: &str, hosted: bool, session_cost_usd: f64) {
        self.emit(json!({
            "type": "usage",
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens,
            "model": model_id,
            "hosted": hosted,
            "session_cost_usd": session_cost_usd,
            // `null` (not 0) when the provider reported nothing -- "we don't
            // know" and "measured zero hits" are different facts, and a
            // client showing "cache 0%" for the former would be inventing a
            // measurement that never happened.
            "cache_hit_tokens": usage.cache_hit_tokens,
            "cache_miss_tokens": usage.cache_miss_tokens,
        }));
    }

    fn retrying(&self, attempt: u32, max: u32, delay: Duration, err: &str) {
        self.emit(json!({
            "type": "retrying",
            "attempt": attempt,
            "max": max,
            "delay_ms": delay.as_millis() as u64,
            "error": err,
        }));
    }

    fn stalled(&self, after_turns: u32) {
        self.emit(json!({"type": "stalled", "after_turns": after_turns}));
    }

    fn output_limit_truncated(&self) {
        self.emit(json!({"type": "output_limit_truncated"}));
    }

    fn model_escalated(&self, from: &str, to: &str, reason: &str) {
        self.emit(json!({
            "type": "model_escalated",
            "from": from,
            "to": to,
            "reason": reason,
        }));
    }

    fn escalation_declined(&self, to: &str, spent: f64, budget: f64) {
        self.emit(json!({
            "type": "escalation_declined",
            "to": to,
            "spent_usd": spent,
            "budget_usd": budget,
        }));
    }

    fn interjected(&self, count: usize) {
        self.emit(json!({"type": "interjected", "count": count}));
    }

    fn context_trimmed(&self, results_elided: usize, tokens_saved: u64) {
        self.emit(json!({
            "type": "context_trimmed",
            "results_elided": results_elided,
            "tokens_saved": tokens_saved,
        }));
    }

    fn compacted(
        &self,
        messages_before: usize,
        messages_after: usize,
        tokens_before: u64,
        summary_cost_usd: Option<f64>,
    ) {
        self.emit(json!({
            "type": "compacted",
            "messages_before": messages_before,
            "messages_after": messages_after,
            "tokens_before": tokens_before,
            "summary_cost_usd": summary_cost_usd,
        }));
    }

    fn stopped_for_budget(&self, spent_usd: f64, budget_usd: f64) {
        self.emit(json!({
            "type": "stopped_for_budget",
            "spent_usd": spent_usd,
            "budget_usd": budget_usd,
        }));
    }

    fn stopped_for_context_limit(&self, estimated_tokens: u64, context_window: u64) {
        self.emit(json!({
            "type": "stopped_for_context_limit",
            "estimated_tokens": estimated_tokens,
            "context_window": context_window,
        }));
    }

    fn tool_progress(&self, tool: &str, message: &str) {
        self.emit(json!({
            "type": "tool_progress",
            "tool": tool,
            "message": message,
        }));
    }
}

/// The pure half of [`JsonUi::emit_history`], split out so the shape can be
/// asserted directly without capturing real process stdout -- `emit()`
/// writes to `io::stdout()` unconditionally, with no injectable sink, so
/// nothing in this file was unit-testable before this existed.
///
/// `None` means "nothing worth emitting": either the session is fresh (just
/// the system message) or, defensively, empty outright.
fn history_wire_messages(messages: &[harness_types::Message]) -> Option<Vec<serde_json::Value>> {
    use harness_types::Role;

    let rest = match messages.first() {
        Some(m) if m.role == Role::System => &messages[1..],
        _ => messages,
    };
    if rest.is_empty() {
        return None;
    }

    Some(
        rest.iter()
            .map(|m| match m.role {
                Role::User => json!({"role": "user", "content": m.content}),
                Role::Assistant => {
                    let mut obj = json!({"role": "assistant", "content": m.content});
                    let map = obj.as_object_mut().expect("assistant is an object");
                    if !m.reasoning.is_empty() {
                        map.insert("reasoning".into(), json!(m.reasoning));
                    }
                    if !m.tool_calls.is_empty() {
                        let calls: Vec<_> = m
                            .tool_calls
                            .iter()
                            .map(|c| json!({"id": c.id, "name": c.name, "args": c.args.get()}))
                            .collect();
                        map.insert("tool_calls".into(), json!(calls));
                    }
                    obj
                }
                Role::Tool => json!({
                    "role": "tool",
                    "tool_call_id": m.tool_call_id,
                    "name": m.name,
                    "content": m.content,
                }),
                // A system message can only appear at index 0, already
                // skipped above -- `Agent::restore` guarantees this (see its
                // own doc comment), so this arm is unreachable in practice
                // rather than a real case needing its own wire shape.
                Role::System => json!({"role": "system", "content": m.content}),
            })
            .collect(),
    )
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use harness_types::{Message, ToolCall};
    use serde_json::value::RawValue;

    fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            args: RawValue::from_string(args.to_string()).unwrap(),
        }
    }

    #[test]
    fn a_fresh_session_is_just_the_system_message_and_emits_nothing() {
        let messages = vec![Message::system("you are HiveMind")];
        assert!(history_wire_messages(&messages).is_none());
    }

    #[test]
    fn an_empty_slice_emits_nothing() {
        assert!(history_wire_messages(&[]).is_none());
    }

    #[test]
    fn the_system_message_is_stripped_but_everything_after_it_is_kept() {
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi there"),
        ];
        let wire = history_wire_messages(&messages).expect("should emit");
        assert_eq!(wire.len(), 2, "system message leaked through: {wire:?}");
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[0]["content"], "hello");
        assert_eq!(wire[1]["role"], "assistant");
        assert_eq!(wire[1]["content"], "hi there");
    }

    /// A real resumed conversation's actual shape: system, user, an
    /// assistant message carrying a tool call with no visible text yet, the
    /// matching tool result, then the assistant's final reply.
    #[test]
    fn a_realistic_tool_using_turn_round_trips_correctly() {
        let mut assistant_step = Message::assistant("");
        assistant_step.tool_calls = vec![tool_call("call_1", "read_file", r#"{"path":"a.rs"}"#)];

        let messages = vec![
            Message::system("sys"),
            Message::user("what does a.rs do?"),
            assistant_step,
            Message::tool_result("call_1", "read_file", "fn main() {}"),
            Message::assistant("a.rs just has an empty main()."),
        ];
        let wire = history_wire_messages(&messages).expect("should emit");
        assert_eq!(wire.len(), 4);

        assert_eq!(wire[1]["role"], "assistant");
        assert_eq!(wire[1]["content"], "");
        let calls = wire[1]["tool_calls"]
            .as_array()
            .expect("tool_calls present");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["name"], "read_file");
        assert_eq!(calls[0]["args"], r#"{"path":"a.rs"}"#);

        assert_eq!(wire[2]["role"], "tool");
        assert_eq!(wire[2]["tool_call_id"], "call_1");
        assert_eq!(wire[2]["name"], "read_file");
        assert_eq!(wire[2]["content"], "fn main() {}");
        // The one deliberate gap: whether this tool call succeeded is never
        // on the wire here, because it was never persisted -- see this
        // function's own doc comment for why guessing it back from the
        // result text would repeat exactly the mistake M1 fixed.
        assert!(
            wire[2].get("is_error").is_none(),
            "a historical tool result must never claim a status it doesn't have"
        );

        assert_eq!(wire[3]["role"], "assistant");
        assert_eq!(wire[3]["content"], "a.rs just has an empty main().");
    }

    #[test]
    fn empty_reasoning_and_tool_calls_are_omitted_not_sent_as_empty() {
        let wire =
            history_wire_messages(&[Message::system("s"), Message::assistant("plain reply")])
                .expect("should emit");
        let obj = wire[0].as_object().unwrap();
        assert!(
            !obj.contains_key("reasoning"),
            "empty reasoning was sent anyway: {obj:?}"
        );
        assert!(
            !obj.contains_key("tool_calls"),
            "empty tool_calls was sent anyway: {obj:?}"
        );
    }

    #[test]
    fn non_empty_reasoning_is_included() {
        let mut m = Message::assistant("the answer");
        m.reasoning = "let me think about this".to_string();
        let wire = history_wire_messages(&[Message::system("s"), m]).expect("should emit");
        assert_eq!(wire[0]["reasoning"], "let me think about this");
    }

    /// Two tool calls in one assistant message (a batched turn) must both
    /// survive, in order -- this is the common case for anything that reads
    /// several files before acting.
    #[test]
    fn multiple_tool_calls_in_one_message_all_survive_in_order() {
        let mut m = Message::assistant("");
        m.tool_calls = vec![
            tool_call("c1", "read_file", r#"{"path":"a.rs"}"#),
            tool_call("c2", "read_file", r#"{"path":"b.rs"}"#),
        ];
        let wire = history_wire_messages(&[Message::system("s"), m]).expect("should emit");
        let calls = wire[0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[1]["id"], "c2");
    }
}
