//! Host callback interface. A CLI, a TUI, or a headless logger all implement
//! this — [`crate::Agent`] never touches a terminal directly.

use std::time::Duration;

use harness_types::Usage;

pub trait Ui: Send + Sync {
    fn turn_started(&self);
    fn assistant_delta(&self, text: &str);
    fn reasoning_delta(&self, text: &str);
    fn assistant_done(&self);
    fn tool_call_pending(&self, _name: &str) {}
    fn tool_start(&self, name: &str, args: &str);
    fn tool_end(
        &self,
        name: &str,
        result: &str,
        is_error: bool,
        cost_usd: f64,
        session_cost_usd: f64,
    );
    fn usage(&self, usage: &Usage, model_id: &str, hosted: bool, session_cost_usd: f64);

    /// Fired before each retry sleep (429/5xx/network hiccup).
    fn retrying(&self, attempt: u32, max: u32, delay: Duration, err: &str);
    fn stalled(&self, _after_turns: u32) {}
    fn output_limit_truncated(&self) {}
    fn validation_required(&self, _changed_files: usize) {}
    fn turns_extended(&self, _turns_used: u32, _new_limit: u32) {}
    fn model_escalated(&self, from: &str, to: &str, reason: &str);
    fn escalation_declined(&self, _to: &str, _spent: f64, _budget: f64) {}
    fn interjected(&self, count: usize);
    fn context_trimmed(&self, _results_elided: usize, _tokens_saved: u64) {}
    fn compacted(
        &self,
        messages_before: usize,
        messages_after: usize,
        tokens_before: u64,
        summary_cost_usd: Option<f64>,
    );

    fn stopped_for_budget(&self, spent_usd: f64, budget_usd: f64);
    fn stopped_for_context_limit(&self, estimated_tokens: u64, context_window: u64);
    fn tool_progress(&self, tool: &str, message: &str);
}
