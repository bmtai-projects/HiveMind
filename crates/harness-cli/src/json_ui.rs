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
    pub fn emit_ready(&self) {
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
        self.emit(json!({"type": "ready", "models": models}));
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

    fn tool_end(&self, name: &str, result: &str, is_error: bool) {
        self.emit(json!({
            "type": "tool_end",
            "name": name,
            "result": result,
            "is_error": is_error,
        }));
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

    fn model_escalated(&self, from: &str, to: &str, reason: &str) {
        self.emit(json!({
            "type": "model_escalated",
            "from": from,
            "to": to,
            "reason": reason,
        }));
    }

    fn interjected(&self, count: usize) {
        self.emit(json!({"type": "interjected", "count": count}));
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
