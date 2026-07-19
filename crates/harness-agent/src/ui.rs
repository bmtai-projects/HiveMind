//! Host callback interface. A CLI, a TUI, or a headless logger all implement
//! this — [`crate::Agent`] never touches a terminal directly.

use std::time::Duration;

use harness_config::{ModelInfo, Tier};
use harness_types::Usage;

pub trait Ui: Send + Sync {
    /// One streamed fragment of the assistant's visible reply.
    fn assistant_delta(&self, text: &str);
    /// One streamed fragment of chain-of-thought (deepseek-v4-pro).
    fn reasoning_delta(&self, text: &str);
    /// Fired once after the assistant's text stream completes (only if any
    /// text was actually streamed — a tool-only turn skips this).
    fn assistant_done(&self);

    fn tool_start(&self, name: &str, args: &str);
    fn tool_end(&self, name: &str, result: &str, is_error: bool);

    /// Fired after every sampled response, whether or not it called tools.
    fn usage(&self, usage: &Usage, tier: Tier, model: &ModelInfo);

    /// Fired before each retry sleep (429/5xx/network hiccup).
    fn retrying(&self, attempt: u32, max: u32, delay: Duration, err: &str);

    /// Fired when the agent bumps Flash → Pro after repeated/failing tool
    /// calls on the current task.
    fn tier_escalated(&self, from: Tier, to: Tier, reason: &str);

    /// Fired after a compaction pass folds older turns into a summary.
    fn compacted(&self, messages_before: usize, messages_after: usize, tokens_before: u64);
}
