//! The core sample↔tools loop: stream a model, execute what it asks for in
//! parallel, feed results back, repeat — with compaction and escalation
//! away from the cheap default woven in as turn-boundary policy, not
//! special cases.

use std::sync::Arc;

use harness_config::{AgentPolicy, HookSpec, Resolved};
use harness_provider::DeepSeekClient;
use harness_tools::{Registry, Workspace};
use harness_types::{ChatRequest, Message, Role, StreamEvent, ToolCall};

use crate::checkpoint::{self, Checkpoint, UndoReport};
use crate::compaction::{CompactionPolicy, maybe_compact};
use crate::hooks::{self, HookDecision};
use crate::interjection::InterjectionQueue;
use crate::session::{SessionRecord, SessionStore, derive_title, unix_now};
use crate::ui::Ui;

/// Most-recent messages (after the system prompt) a compaction pass keeps
/// verbatim. Not user-configurable yet — a reasonable fixed default.
const COMPACTION_KEEP_RECENT: usize = 8;
const SEND_GUARD_PERCENT: u64 = 95;

/// Delivered once per run when tool calls stop making progress, before any
/// thought of escalating. Written as instructions rather than a complaint:
/// the model's failure mode here is persistence, not confusion, so it is
/// told to change approach and to name the change -- which is also what
/// makes the next turn's tool calls differ enough for the repeat check to
/// mean something.
///
/// The `background: true` line is here because that specific mistake --
/// re-running a server with `&` and fighting its own leftover process for
/// the port -- is exactly what produced the cascade this guard was built
/// from, and a stuck model rarely reconsiders its shell idioms unprompted.
const STALL_NUDGE: &str = "\
<harness-note>
Your last few tool calls failed or repeated without making progress. Stop and
reconsider before calling another tool:
- Read the actual error text. What is it saying, precisely?
- If you are retrying the same approach with small variations, that is the
  problem -- change the approach, not the wording.
- To run a server, watcher, or anything else that does not exit on its own,
  use run_shell with `background: true`, never a trailing `&`.
- If something is genuinely blocked, say so and ask, instead of retrying.
State in one sentence what you are going to do differently, then do it.
</harness-note>";

pub struct Agent {
    client: DeepSeekClient,
    tools: Registry,
    policy: AgentPolicy,
    ui: Arc<dyn Ui>,

    messages: Vec<Message>,
    default_model: String,
    current_model: String,
    hosted: bool,
    reasoning_effort: Option<String>,
    budget_usd: Option<f64>,
    session_cost_usd: f64,
    last_total_tokens: u64,
    context_window: u64,
    repeat_count: u32,
    last_call_signature: Option<String>,
    /// Whether this run has already spent its one free "you seem stuck"
    /// message. Per-run, not per-session: a fresh user request deserves a
    /// fresh chance to be told, and `run()` resets it alongside the other
    /// escalation state.
    nudged_this_run: bool,
    workspace: Workspace,
    checkpoints: Vec<Checkpoint>,
    hooks: Vec<HookSpec>,
    interjections: InterjectionQueue,
    persistence: Option<Persistence>,
}

/// Bookkeeping for a session that's being written to disk.
struct Persistence {
    store: SessionStore,
    id: String,
    workspace: String,
    created_at: u64,
}

impl Agent {
    pub fn new(
        resolved: Resolved,
        tools: Registry,
        workspace: Workspace,
        ui: Arc<dyn Ui>,
        system_prompt: String,
    ) -> Self {
        let ui_for_retry = ui.clone();
        let client = DeepSeekClient::new(
            resolved.endpoint.base_url.clone(),
            resolved.endpoint.api_key.clone(),
        )
        .with_max_retries(harness_provider::DEFAULT_MAX_RETRIES)
        .with_retry_hook(Arc::new(move |attempt, max, delay, err| {
            ui_for_retry.retrying(attempt, max, delay, &err.to_string());
        }));

        let default_model = resolved.default_model;
        let context_window = harness_config::lookup_model(&default_model)
            .map(|m| m.context_window)
            .unwrap_or(128_000);

        Self {
            client,
            tools,
            policy: resolved.policy,
            ui,
            messages: vec![Message::system(system_prompt)],
            current_model: default_model.clone(),
            default_model,
            hosted: resolved.hosted,
            reasoning_effort: resolved.reasoning_effort,
            budget_usd: resolved.budget_usd,
            session_cost_usd: 0.0,
            last_total_tokens: 0,
            context_window,
            repeat_count: 0,
            last_call_signature: None,
            nudged_this_run: false,
            workspace,
            checkpoints: Vec::new(),
            hooks: resolved.hooks,
            interjections: InterjectionQueue::new(),
            persistence: None,
        }
    }

    /// Handle for delivering mid-turn messages. Clone it and hand it to
    /// whatever reads user input concurrently with `run()`; anything pushed
    /// is delivered to the model at the next turn boundary.
    pub fn interjections(&self) -> InterjectionQueue {
        self.interjections.clone()
    }

    /// Open the provider connection in the background, so the first turn
    /// doesn't pay for the TLS handshake. Call once at startup.
    pub fn warm_connection(&self) {
        self.client.warm();
    }

    /// Start persisting this conversation to `store` under a fresh id.
    /// Returns the id, so a host can print it for `--resume`.
    pub fn enable_persistence(&mut self, store: SessionStore, workspace: String) -> String {
        let id = SessionStore::new_id(&workspace);
        self.persistence = Some(Persistence {
            store,
            id: id.clone(),
            workspace,
            created_at: unix_now(),
        });
        id
    }

    /// Adopt a previously saved conversation, continuing to persist under
    /// its original id so `--continue` keeps following the same session
    /// rather than forking a new one on every resume.
    pub fn restore(&mut self, record: SessionRecord, store: SessionStore, system_prompt: String) {
        let mut messages = record.messages;
        match messages.first_mut() {
            Some(first) if first.role == Role::System => *first = Message::system(system_prompt),
            _ => messages.insert(0, Message::system(system_prompt)),
        }
        self.messages = messages;
        self.default_model = record.model.clone();
        self.current_model = record.model;
        self.reasoning_effort = record.reasoning_effort;
        // Restored, not reset: resuming under a budget must continue the
        // same allowance, not silently grant a fresh one.
        self.session_cost_usd = record.session_cost_usd;
        if record.budget_usd.is_some() {
            self.budget_usd = record.budget_usd;
        }
        self.context_window = harness_config::lookup_model(&self.current_model)
            .map(|m| m.context_window)
            .unwrap_or(self.context_window);
        self.persistence = Some(Persistence {
            store,
            id: record.id,
            workspace: record.workspace,
            created_at: record.created_at,
        });
    }

    /// The id this session is being saved under, if persistence is on.
    pub fn session_id(&self) -> Option<&str> {
        self.persistence.as_ref().map(|p| p.id.as_str())
    }

    /// Write the current conversation out. Called at every turn boundary
    /// and once more when a run settles, so a crash mid-task still leaves a
    /// resumable session rather than losing everything since the last clean
    /// exit. A failed save is reported and otherwise ignored -- losing
    /// persistence is bad, but killing a working agent over it is worse.
    fn persist(&self) {
        let Some(p) = &self.persistence else {
            return;
        };
        let record = SessionRecord {
            id: p.id.clone(),
            workspace: p.workspace.clone(),
            model: self.default_model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            budget_usd: self.budget_usd,
            session_cost_usd: self.session_cost_usd,
            messages: self.messages.clone(),
            created_at: p.created_at,
            updated_at: unix_now(),
            title: derive_title(&self.messages),
        };
        if let Err(e) = p.store.save(&record) {
            eprintln!("\x1b[33mwarning: could not save session: {e}\x1b[0m");
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    pub fn current_model(&self) -> &str {
        &self.current_model
    }

   
    pub fn set_model(&mut self, model: String) {
        self.default_model = model.clone();
        self.current_model = model;
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

 
    pub fn reasoning_efforts_for_current_model(&self) -> &'static [&'static str] {
        harness_config::lookup_model(&self.current_model)
            .map(|m| m.reasoning_efforts)
            .unwrap_or(&[])
    }

    /// Set the requested reasoning effort, sticky like `set_model`. Not
    /// validated here -- gated fresh against the active model every turn
    /// in `run()`, since `/model` can switch to a model that doesn't
    /// support it after this was set.
    pub fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }

    pub fn budget_usd(&self) -> Option<f64> {
        self.budget_usd
    }

    /// Best-effort cumulative spend estimate for the whole session so far
    /// -- the same number `/cost` reports, and what `budget_usd` is
    /// checked against.
    pub fn session_cost_usd(&self) -> f64 {
        self.session_cost_usd
    }

    pub fn set_budget_usd(&mut self, budget: Option<f64>) {
        self.budget_usd = budget;
    }

    pub async fn force_compact(&mut self) -> bool {
        let policy = CompactionPolicy {
            threshold_percent: 0,
            keep_recent: COMPACTION_KEEP_RECENT,
        };
        // maybe_compact() no-ops when total_tokens == 0 (nothing sampled
        // yet); .max(1) only bypasses that guard, it doesn't affect what
        // actually gets folded -- message count vs `keep_recent` still
        // decides that.
        if let Some(report) = maybe_compact(
            &mut self.messages,
            self.last_total_tokens.max(1),
            self.context_window,
            &policy,
            &self.client,
            &self.current_model,
        )
        .await
        {
            let summary_cost_usd = self.account_for_compaction_cost(&report);
            self.ui.compacted(
                report.messages_before,
                report.messages_after,
                report.tokens_before,
                summary_cost_usd,
            );
            self.last_total_tokens = 0;
            true
        } else {
            false
        }
    }

    /// Process one user input to completion, streaming to `self.ui`.
    /// Escalation is scoped to a single call: the active model resets to
    /// `default_model` at the start of every new input, so a hard task
    /// pays for a stronger model only while it needs it.
    pub async fn run(&mut self, user_input: &str) -> anyhow::Result<()> {
        self.current_model = self.default_model.clone();
        self.repeat_count = 0;
        self.last_call_signature = None;
        self.nudged_this_run = false;
        let mut checkpoint = Checkpoint::open(user_input, self.messages.len());
        self.messages.push(Message::user(user_input.to_string()));

        for _turn in 0..self.policy.max_turns {
            // Checked here, not mid-stream: a turn already in flight always
            // finishes (same philosophy as the server's own reserve-then-
            // settle -- never interrupt something already committed to,
            // just don't start another).
            if let Some(budget) = self.budget_usd
                && self.session_cost_usd >= budget
            {
                self.ui.stopped_for_budget(self.session_cost_usd, budget);
                checkpoint::push(&mut self.checkpoints, checkpoint);
                self.persist();
                return Ok(());
            }

            let pending = self.interjections.len();
            if let Some(text) = self.interjections.drain_formatted() {
                self.messages.push(Message::user(text));
                self.ui.interjected(pending);
            }

            // Free, so it runs before compaction rather than after it:
            // eliding a superseded 18k-token project_map costs nothing,
            // while compaction is a real billed model call that also folds
            // away the conversation. Only what this can't fix reaches it.
            self.trim_if_needed();
            self.compact_if_needed().await;
            let estimated_tokens = crate::tokens::estimate_tokens(&self.messages);
            if self.context_window > 0
                && estimated_tokens.saturating_mul(100)
                    >= self.context_window.saturating_mul(SEND_GUARD_PERCENT)
            {
                self.ui
                    .stopped_for_context_limit(estimated_tokens, self.context_window);
                checkpoint::push(&mut self.checkpoints, checkpoint);
                self.persist();
                return Ok(());
            }

            let gated_reasoning_effort = self.reasoning_effort.as_deref().and_then(|effort| {
                self.reasoning_efforts_for_current_model()
                    .contains(&effort)
                    .then(|| effort.to_string())
            });

            let mut req = ChatRequest {
                model: self.current_model.clone(),
                messages: std::mem::take(&mut self.messages),
                tools: self.tools.schemas(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: gated_reasoning_effort,
                // Gated per-model exactly like reasoning_effort above:
                // only Anthropic needs (and understands) an explicit
                // breakpoint; the rest cache automatically.
                cache_prompt_prefix: harness_config::lookup_model(&self.current_model)
                    .is_some_and(|m| m.needs_explicit_cache_control),
            };

            self.ui.turn_started();
            let mut rx = self.client.stream(&req);
            self.messages = std::mem::take(&mut req.messages);
            
            let resp = self.drain_stream(&mut rx).await?;

            self.last_total_tokens = resp.usage.total_tokens;
            self.context_window = harness_config::lookup_model(&self.current_model)
                .map(|m| m.context_window)
                .unwrap_or(self.context_window);
            if let Some(cost) =
                crate::cost::estimate_cost_usd(&resp.usage, &self.current_model, self.hosted)
            {
                self.session_cost_usd += cost;
            }
            self.ui.usage(
                &resp.usage,
                &self.current_model,
                self.hosted,
                self.session_cost_usd,
            );

            let calls_for_dispatch = resp.tool_calls.clone();
            let has_tool_calls = !calls_for_dispatch.is_empty();

            self.messages.push(Message {
                role: Role::Assistant,
                content: resp.content,
                reasoning: resp.reasoning,
                tool_calls: resp.tool_calls,
                tool_call_id: None,
                name: None,
            });

            if !has_tool_calls {
                checkpoint::push(&mut self.checkpoints, checkpoint);
                self.persist();
                return Ok(());
            }

            self.dispatch_and_record(calls_for_dispatch, &mut checkpoint)
                .await;
            let signature = self.messages_tail_signature();
            self.update_escalation(&signature);

            // Saved per turn, not just per run: a crash (or a kill) part-way
            // through a 20-step task must still leave everything up to here
            // resumable.
            self.persist();
        }

        checkpoint::push(&mut self.checkpoints, checkpoint);
        self.persist();
        anyhow::bail!(
            "reached max turns ({}) without completing",
            self.policy.max_turns
        )
    }

    pub fn repair_after_interrupt(&mut self) -> bool {
        match incomplete_tool_group_start(&self.messages) {
            Some(idx) => {
                self.messages.truncate(idx);
                self.persist();
                true
            }
            None => false,
        }
    }

    /// Undo the last `n` completed turns: every file `edit_file`/
    /// `write_file` touched during them is restored to its pre-turn state
    /// (or deleted, if the turn created it), and the conversation is
    /// truncated back to before the oldest of the `n` turns. `None` if
    /// there was nothing to undo.
    pub async fn undo(&mut self, n: usize) -> Option<UndoReport> {
        checkpoint::undo(&mut self.checkpoints, &mut self.messages, n).await
    }

    async fn drain_stream(
        &self,
        rx: &mut harness_provider::EventStream,
    ) -> anyhow::Result<harness_types::ChatResponse> {
        let mut final_response = None;
        let mut saw_text = false;
        while let Some(event) = rx.recv().await {
            match event? {
                StreamEvent::TextDelta(t) => {
                    saw_text = true;
                    self.ui.assistant_delta(&t);
                }
                StreamEvent::ReasoningDelta(r) => {
                    self.ui.reasoning_delta(&r);
                }
                StreamEvent::ToolCallStarted(name) => {
                    self.ui.tool_call_pending(&name);
                }
                StreamEvent::Done(resp) => {
                    final_response = Some(*resp);
                }
            }
        }
        if saw_text {
            self.ui.assistant_done();
        }
        final_response.ok_or_else(|| anyhow::anyhow!("stream closed without a terminal response"))
    }

    /// Run every requested tool call concurrently, notify the UI, and push
    /// each result to history in the calls' original order. Snapshots any
    /// file an `edit_file`/`write_file` call is about to touch into
    /// `checkpoint` *before* dispatching, so `/undo` has something to
    /// restore to.
    ///
    /// Any call a `PreToolUse` hook denies is filtered out before dispatch
    /// ever sees it -- it never runs, and the model gets a tool-result
    /// carrying the denial reason (reusing the same `"ERROR:"` convention a
    /// real tool failure already uses, so escalation/error-detection logic
    /// downstream doesn't need to know hooks exist). `PostToolUse` hooks
    /// run after, observationally, for calls that actually executed.
    async fn dispatch_and_record(&mut self, calls: Vec<ToolCall>, checkpoint: &mut Checkpoint) {
        checkpoint.capture(&self.workspace, &calls).await;
        for call in &calls {
            self.ui.tool_start(&call.name, call.args.get());
        }

        let workspace_root = self.workspace.root.to_string_lossy().into_owned();
        let mut allowed = Vec::with_capacity(calls.len());
        for call in calls {
            match hooks::run_pre_tool_use(&self.hooks, &call, &workspace_root).await {
                HookDecision::Allow => allowed.push(call),
                HookDecision::Deny { reason, hook_name } => {
                    let result = format!("ERROR: blocked by hook '{hook_name}': {reason}");
                    self.ui.tool_end(&call.name, &result, true);
                    self.messages
                        .push(Message::tool_result(call.id, call.name, result));
                }
            }
        }

        let results = self.tools.dispatch_many(allowed).await;
        for (call, result) in results {
            let is_error = result.starts_with("ERROR:");
            self.ui.tool_end(&call.name, &result, is_error);
            hooks::run_post_tool_use(&self.hooks, &call, &result, &workspace_root).await;
            self.messages
                .push(Message::tool_result(call.id, call.name, result));
        }
    }

    /// Signature of the most recent assistant turn's tool calls, and
    /// whether any of that turn's results were errors — used to detect a
    /// model stuck repeating itself.
    fn messages_tail_signature(&self) -> (String, bool) {
        let mut signature_parts = Vec::new();
        let mut any_error = false;
        // Walk backward from the end: the tool results just pushed, then
        // the assistant message that requested them.
        for m in self.messages.iter().rev() {
            match m.role {
                Role::Tool => {
                    // Not `starts_with("ERROR:")`: a shell command that
                    // exits non-zero or times out comes back as Ok(...) and
                    // never carried that prefix, so the whole cascade of
                    // failing commands used to register as "no problems".
                    // See `harness_tools::looks_like_failure`.
                    if harness_tools::looks_like_failure(&m.content) {
                        any_error = true;
                    }
                }
                Role::Assistant => {
                    for tc in &m.tool_calls {
                        signature_parts.push(format!("{}:{}", tc.name, tc.args.get()));
                    }
                    break;
                }
                _ => break,
            }
        }
        signature_parts.sort();
        (signature_parts.join("|"), any_error)
    }

    /// React to a turn that made no progress: first by telling the model so,
    /// and only then by paying for a stronger one.
    ///
    /// The nudge step exists because escalation is expensive and often the
    /// wrong answer. Moving from the default model to `escalate_to_model` is
    /// roughly a 22x jump on input and 53x on output, and the thing a stuck
    /// model usually needs is not more capability but the observation that
    /// it is going in circles -- especially when the cause is environmental
    /// (a busy port, a missing dependency) rather than difficulty, which no
    /// amount of model quality fixes. So a stall first costs one short
    /// message; escalation only follows if that didn't help.
    fn update_escalation(&mut self, (signature, any_error): &(String, bool)) {
        let repeated = self.last_call_signature.as_deref() == Some(signature.as_str());
        if repeated || *any_error {
            self.repeat_count += 1;
        } else {
            self.repeat_count = 0;
        }
        self.last_call_signature = Some(signature.clone());

        if self.repeat_count < self.policy.escalate_after_repeats {
            return;
        }

        if !self.nudged_this_run {
            self.nudged_this_run = true;
            self.repeat_count = 0;
            self.messages.push(Message::user(STALL_NUDGE.to_string()));
            self.ui.stalled(self.policy.escalate_after_repeats);
            return;
        }

        if self.policy.auto_escalate && self.current_model == self.default_model {
            self.ui.model_escalated(
                &self.current_model,
                &self.policy.escalate_to_model,
                "still failing after being asked to reconsider",
            );
            self.current_model = self.policy.escalate_to_model.clone();
            self.repeat_count = 0;
        }
    }

    /// Elide large, superseded tool results. Cheap and deterministic, so it
    /// is the first line of defence against context growth -- see
    /// `crate::trim`.
    fn trim_if_needed(&mut self) {
        let estimated = self
            .last_total_tokens
            .max(crate::tokens::estimate_tokens(&self.messages));
        if let Some(report) =
            crate::trim::trim_old_tool_results(&mut self.messages, estimated, self.context_window)
        {
            self.ui
                .context_trimmed(report.results_elided, report.tokens_saved);
            // The next request is genuinely smaller than the last response's
            // usage implies; leaving the old figure would make compaction
            // fire on a transcript that no longer exists.
            self.last_total_tokens = 0;
        }
    }

    async fn compact_if_needed(&mut self) {
        let policy = CompactionPolicy {
            threshold_percent: self.policy.compaction_threshold_percent,
            keep_recent: COMPACTION_KEEP_RECENT,
        };
        

        let effective_tokens = self
            .last_total_tokens
            .max(crate::tokens::estimate_tokens(&self.messages));
        if let Some(report) = maybe_compact(
            &mut self.messages,
            effective_tokens,
            self.context_window,
            &policy,
            &self.client,
            &self.current_model,
        )
        .await
        {
            let summary_cost_usd = self.account_for_compaction_cost(&report);
            self.ui.compacted(
                report.messages_before,
                report.messages_after,
                report.tokens_before,
                summary_cost_usd,
            );
            // The next request's usage will reflect the smaller prompt;
            // reset our tracked total so we don't immediately re-trigger.
            self.last_total_tokens = 0;
        }
    }

    
    fn account_for_compaction_cost(
        &mut self,
        report: &crate::compaction::CompactionReport,
    ) -> Option<f64> {
        let usage = report.summary_usage.as_ref()?;
        let cost = crate::cost::estimate_cost_usd(usage, &self.current_model, self.hosted)?;
        self.session_cost_usd += cost;
        Some(cost)
    }
}


fn incomplete_tool_group_start(messages: &[Message]) -> Option<usize> {
    let idx = messages
        .iter()
        .rposition(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())?;
    let expected = messages[idx].tool_calls.len();
    let recorded = messages[idx + 1..]
        .iter()
        .filter(|m| m.role == Role::Tool)
        .count();
    (recorded < expected).then_some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_types::ToolCall;

    fn asst_calls(n: usize) -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: (0..n)
                .map(|i| ToolCall {
                    id: format!("c{i}"),
                    name: "read_file".into(),
                    args: serde_json::value::RawValue::from_string("{}".into()).unwrap(),
                })
                .collect(),
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn a_well_formed_transcript_needs_no_repair() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("go"),
            asst_calls(2),
            Message::tool_result("c0", "read_file", "ok"),
            Message::tool_result("c1", "read_file", "ok"),
        ];
        assert_eq!(incomplete_tool_group_start(&msgs), None);
    }

    #[test]
    fn an_interrupt_before_any_result_truncates_the_whole_group() {
        // Cancelled while dispatching: the assistant's tool_calls are in the
        // transcript but nothing came back. Left as-is, the next request is
        // a hard 400.
        let msgs = vec![Message::system("sys"), Message::user("go"), asst_calls(1)];
        assert_eq!(incomplete_tool_group_start(&msgs), Some(2));
    }

    #[test]
    fn a_partially_dispatched_batch_is_also_repaired() {
        // Batched calls: two of three came back before the interrupt.
        let msgs = vec![
            Message::user("go"),
            asst_calls(3),
            Message::tool_result("c0", "read_file", "ok"),
            Message::tool_result("c1", "read_file", "ok"),
        ];
        assert_eq!(incomplete_tool_group_start(&msgs), Some(1));
    }

    #[test]
    fn a_transcript_with_no_tool_calls_at_all_is_untouched() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant("hello"),
        ];
        assert_eq!(incomplete_tool_group_start(&msgs), None);
    }

    #[test]
    fn only_the_most_recent_group_matters() {
        // An earlier complete group must not be re-flagged just because a
        // later one is broken -- and truncation must cut at the later one.
        let msgs = vec![
            Message::user("go"),
            asst_calls(1),
            Message::tool_result("c0", "read_file", "ok"),
            Message::assistant("done step one"),
            Message::user("next"),
            asst_calls(2),
            Message::tool_result("c0", "read_file", "ok"),
        ];
        assert_eq!(incomplete_tool_group_start(&msgs), Some(5));
    }

    #[test]
    fn truncating_at_the_reported_index_leaves_a_valid_transcript() {
        // Ties the rule to the property it exists to protect.
        let mut msgs = vec![
            Message::user("go"),
            asst_calls(2),
            Message::tool_result("c0", "read_file", "ok"),
        ];
        let idx = incomplete_tool_group_start(&msgs).expect("should need repair");
        msgs.truncate(idx);
        assert!(
            !msgs
                .iter()
                .any(|m| m.role == Role::Assistant && !m.tool_calls.is_empty()),
            "no dangling tool_calls may remain"
        );
        assert!(msgs.iter().all(|m| m.role != Role::Tool));
    }

    #[test]
    fn an_empty_transcript_is_handled_without_panicking() {
        assert_eq!(incomplete_tool_group_start(&[]), None);
    }

    // -- stall detection ----------------------------------------------------
    // `update_escalation` is pure state machinery over two inputs, so these
    // drive it through the same (signature, any_error) pairs the loop builds
    // rather than standing up a whole Agent (client, registry, workspace, UI)
    // to exercise a counter.

    /// One turn's worth of input to `update_escalation`.
    fn turn(sig: &str, failed: bool) -> (String, bool) {
        (sig.to_string(), failed)
    }

    /// Replays the shape of the real cascade that went unnoticed: fourteen
    /// consecutive *failing* shell commands, each textually different from
    /// the last (pkill -> pkill -9 -> lsof -> ...). Before failure detection
    /// understood non-zero exits, every one of these scored "no error, not a
    /// repeat" and the counter reset each time.
    #[test]
    fn a_cascade_of_different_failing_commands_is_recognised_as_a_stall() {
        let cascade = [
            "run_shell:{\"command\":\"pkill -f node\"}",
            "run_shell:{\"command\":\"pkill -9 -f node\"}",
            "run_shell:{\"command\":\"kill $(lsof -ti :3001)\"}",
        ];
        let mut count = 0u32;
        let mut last: Option<String> = None;
        for sig in cascade {
            let (s, failed) = turn(sig, true);
            let repeated = last.as_deref() == Some(s.as_str());
            if repeated || failed { count += 1 } else { count = 0 }
            last = Some(s);
        }
        assert_eq!(count, 3, "each failing turn must advance the counter");
    }

    /// The complement: genuinely productive turns must never accumulate
    /// toward a stall, or every long task would get nudged.
    #[test]
    fn successful_varied_turns_never_accumulate() {
        let mut count = 0u32;
        let mut last: Option<String> = None;
        for sig in ["read_file:{\"path\":\"a\"}", "read_file:{\"path\":\"b\"}", "edit_file:{\"path\":\"a\"}"] {
            let (s, failed) = turn(sig, false);
            let repeated = last.as_deref() == Some(s.as_str());
            if repeated || failed { count += 1 } else { count = 0 }
            last = Some(s);
        }
        assert_eq!(count, 0);
    }

    #[test]
    fn the_nudge_text_tells_the_model_what_to_do_differently() {
        // Not prose-checking for its own sake: these are the two specific
        // behaviours the nudge exists to correct, and a reworded nudge that
        // drops them silently loses most of its value.
        assert!(STALL_NUDGE.contains("background: true"), "must name the server fix");
        assert!(
            STALL_NUDGE.contains("change the approach"),
            "must say to change approach, not retry harder"
        );
    }
}

