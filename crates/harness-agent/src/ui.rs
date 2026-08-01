//! Host callback interface. A CLI, a TUI, or a headless logger all implement
//! this — [`crate::Agent`] never touches a terminal directly.

use std::time::Duration;

use harness_types::Usage;

pub trait Ui: Send + Sync {
    /// Fired once per model call, immediately before it's sent -- before
    /// any network activity, before any bytes have come back. Unlike
    /// `reasoning_delta` (which only ever fires for models that actually
    /// stream chain-of-thought, i.e. most prompts on most models never
    /// trigger it), this is unconditional: it's the one signal a host UI
    /// can rely on to show *something* the instant a request goes out,
    /// rather than sitting silent through the network round-trip and
    /// looking stalled.
    fn turn_started(&self);

    /// One streamed fragment of the assistant's visible reply.
    fn assistant_delta(&self, text: &str);
    /// One streamed fragment of chain-of-thought (reasoning-capable models
    /// only, e.g. deepseek-v4-pro).
    fn reasoning_delta(&self, text: &str);
    /// Fired once after the assistant's text stream completes (only if any
    /// text was actually streamed — a tool-only turn skips this).
    fn assistant_done(&self);

    /// A tool call's name arrived, but its arguments are still streaming and
    /// nothing has run yet. Fires between `turn_started` and `tool_start`.
    /// Optional: `tool_start` stays the authoritative "now running" event.
    fn tool_call_pending(&self, _name: &str) {}

    fn tool_start(&self, name: &str, args: &str);
    fn tool_end(&self, name: &str, result: &str, is_error: bool);

    /// Fired after every sampled response, whether or not it called tools.
    /// `hosted` says whether `model_id` is billed through HiveMind's hosted
    /// margin or paid directly to the upstream provider — see
    /// `harness_config::HOSTED_MARKUP_MULTIPLIER`. `session_cost_usd` is
    /// `Agent`'s own running total (already includes this turn) — the UI
    /// displays it rather than keeping a second, redundant copy.
    fn usage(&self, usage: &Usage, model_id: &str, hosted: bool, session_cost_usd: f64);

    /// Fired before each retry sleep (429/5xx/network hiccup).
    fn retrying(&self, attempt: u32, max: u32, delay: Duration, err: &str);

    /// Fired when consecutive tool calls stopped making progress and the
    /// agent told the model so, in place of silently retrying (or of
    /// immediately paying for a stronger model -- escalation only follows
    /// if this doesn't help). Worth surfacing because it explains an
    /// otherwise-invisible extra message in the transcript, and because a
    /// stall is usually the first sign of something the user may want to
    /// step in on. Defaulted: a host that ignores it is still correct.
    fn stalled(&self, _after_turns: u32) {}

    /// Fired when the agent bumps away from the cheap "hivemind" default
    /// after repeated/failing tool calls on the current task.
    fn model_escalated(&self, from: &str, to: &str, reason: &str);

    /// Fired when queued mid-turn messages were handed to the model at a
    /// turn boundary (see `crate::InterjectionQueue`). Worth surfacing
    /// because delivery is deliberately deferred: the user typed at some
    /// arbitrary earlier moment and needs to know the message actually
    /// landed, and when.
    fn interjected(&self, count: usize);

    /// Fired when large, superseded tool results were dropped from history
    /// to free context (see `crate::trim`). Distinct from `compacted`:
    /// nothing was summarized and no model call was made, so this is free
    /// and the conversation itself is untouched. Defaulted to a no-op --
    /// a host that ignores it is still correct, just quieter.
    fn context_trimmed(&self, _results_elided: usize, _tokens_saved: u64) {}

    /// Fired after a compaction pass folds older turns into a summary.
    /// `summary_cost_usd` is the cost of the summarization call itself (the
    /// compactor samples a real model to write the summary) -- `None` when
    /// that call failed outright (no usage to bill) rather than when it was
    /// merely free. Already folded into the `session_cost_usd` the next
    /// `usage()`/`stopped_for_budget()` call reports; surfaced here too so
    /// it's visible at the moment it's actually incurred, not just averaged
    /// into the following turn's total.
    fn compacted(
        &self,
        messages_before: usize,
        messages_after: usize,
        tokens_before: u64,
        summary_cost_usd: Option<f64>,
    );

    /// Fired when a session budget is set and cumulative estimated spend
    /// has reached it -- the agent stops *before* starting another turn,
    /// never mid-stream, so whatever was already in flight always finishes
    /// (see `Agent::run`).
    fn stopped_for_budget(&self, spent_usd: f64, budget_usd: f64);

    /// Fired when the next request, even after a compaction attempt, is
    /// still estimated to be too large for the active model's context
    /// window -- e.g. a single pasted input bigger than the window itself,
    /// which compaction cannot help with since it only folds *older* turns,
    /// not the one just sent. A clean stop instead of letting the request
    /// go out and get rejected by the provider with a confusing wire error.
    fn stopped_for_context_limit(&self, estimated_tokens: u64, context_window: u64);

    /// Fired zero or more times *during* a single tool call, between its
    /// `tool_start` and `tool_end`, for a call whose duration can otherwise
    /// leave a host UI with no signal to distinguish "still working" from
    /// "hung" -- e.g. `semantic_search` building an index across several
    /// slow network-backed embedding batches. Purely informational: no tool
    /// is required to call this, and none did before hosted embeddings.
    fn tool_progress(&self, tool: &str, message: &str);
}
