//! Host callback interface. A CLI, a TUI, or a headless logger all implement
//! this — [`crate::Agent`] never touches a terminal directly.

use std::time::Duration;

use harness_types::Usage;

pub trait Ui: Send + Sync {
    /// One streamed fragment of the assistant's visible reply.
    fn assistant_delta(&self, text: &str);
    /// One streamed fragment of chain-of-thought (reasoning-capable models
    /// only, e.g. deepseek-v4-pro).
    fn reasoning_delta(&self, text: &str);
    /// Fired once after the assistant's text stream completes (only if any
    /// text was actually streamed — a tool-only turn skips this).
    fn assistant_done(&self);

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

    /// Fired when the agent bumps away from the cheap "hivemind" default
    /// after repeated/failing tool calls on the current task.
    fn model_escalated(&self, from: &str, to: &str, reason: &str);

    /// Fired after a compaction pass folds older turns into a summary.
    fn compacted(&self, messages_before: usize, messages_after: usize, tokens_before: u64);

    /// Fired when a session budget is set and cumulative estimated spend
    /// has reached it -- the agent stops *before* starting another turn,
    /// never mid-stream, so whatever was already in flight always finishes
    /// (see `Agent::run`).
    fn stopped_for_budget(&self, spent_usd: f64, budget_usd: f64);
}
