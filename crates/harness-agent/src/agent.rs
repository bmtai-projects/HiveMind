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

/// Last-resort ceiling, as a percent of the active model's context window,
/// past which a request is refused rather than sent. Deliberately higher
/// than `compaction_threshold_percent` (75% default) -- compaction is the
/// first line of defense and should already have acted well before this;
/// this exists only for what compaction *can't* fix (no older turns left
/// to fold) or when the estimate itself runs a little hot.
const SEND_GUARD_PERCENT: u64 = 95;

pub struct Agent {
    client: DeepSeekClient,
    tools: Registry,
    policy: AgentPolicy,
    ui: Arc<dyn Ui>,

    messages: Vec<Message>,
    /// Sticky across `run()` calls; changed only by an explicit `/model`
    /// command. `current_model` resets to this at the start of every input.
    default_model: String,
    current_model: String,
    /// Whether `default_model` is billed through HiveMind's hosted margin
    /// (true) or paid directly to the upstream provider via a BYOK key
    /// (false) -- only affects the live cost readout, see `crate::ui::Ui::usage`.
    hosted: bool,
    /// User intent, not necessarily what gets sent -- gated per-model
    /// against `harness_config::lookup_model(...).reasoning_efforts` fresh
    /// every turn in `run()`, since `/model` can switch to a model that
    /// doesn't support whatever was requested (or doesn't support the
    /// parameter at all). Sticky like `default_model`.
    reasoning_effort: Option<String>,
    /// Hard cap on cumulative estimated USD spend across the *whole*
    /// session (every `run()` call, not just the current one) -- matches
    /// what `/cost` already reports as "session cost so far". Checked at
    /// the start of every turn, not mid-stream: the in-flight turn is
    /// always allowed to finish (mirrors the server's own reserve-then-
    /// settle philosophy -- never interrupt something already committed
    /// to, just don't start another). `None` means unbounded, the default.
    budget_usd: Option<f64>,
    /// Running total this estimate is checked against. Computed from the
    /// exact same formula the UI's own readout uses (`crate::cost`), so
    /// the two can never quietly disagree -- but this is HiveMind's own
    /// best-effort client-side estimate, not what the server actually
    /// bills; see `crate::cost::estimate_cost_usd`'s doc comment.
    session_cost_usd: f64,
    last_total_tokens: u64,
    context_window: u64,

    /// Doom-loop guard: consecutive turns whose tool calls were identical
    /// to the previous turn's, or produced a tool error.
    repeat_count: u32,
    last_call_signature: Option<String>,

    /// Used only to snapshot/restore files for `/undo` -- reuses the exact
    /// same path-escape check the file tools themselves enforce.
    workspace: Workspace,
    /// One entry per completed `run()` call, oldest first. See
    /// `crate::checkpoint` for why `run_shell` doesn't participate.
    checkpoints: Vec<Checkpoint>,
    /// From `config.toml`'s `[[hooks]]`. Empty unless the user configured
    /// any -- see `crate::hooks`.
    hooks: Vec<HookSpec>,

    /// Messages the user sent *while* a turn was already running, delivered
    /// at the next turn boundary. Cloneable handle: hosts hand a clone to
    /// whatever reads input concurrently (see `crate::interjection`).
    interjections: InterjectionQueue,

    /// Where this conversation is persisted, if anywhere. `None` disables
    /// persistence entirely (used by tests and one-shot `-p` runs, which
    /// have nothing worth resuming).
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
    ///
    /// The **system prompt is deliberately not restored**: `messages[0]` is
    /// replaced with the current one. A resumed session runs on today's
    /// binary, whose tools and guidance may differ from whenever the
    /// session started -- replaying a stale prompt would describe tools
    /// that no longer exist (or omit ones that now do).
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

    /// Set the model this session runs on, effective immediately and sticky
    /// across future inputs (unlike auto-escalation, which resets to the
    /// configured default at the start of every new `run()` call). Used by
    /// an explicit user command (e.g. a REPL `/model` command), not by the
    /// doom-loop guard.
    pub fn set_model(&mut self, model: String) {
        self.default_model = model.clone();
        self.current_model = model;
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    /// Which `reasoning_effort` values the *current* model actually
    /// accepts -- empty means it either doesn't support the parameter, or
    /// (for a couple of models that reason unconditionally but only expose
    /// OpenRouter's richer `reasoning: {...}` object) isn't wired up here.
    /// Callers (the REPL's `/reasoning` command) should validate against
    /// this before calling `set_reasoning_effort`, since this type doesn't
    /// -- same trust boundary as `set_model` not validating BYOK ids.
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
                    if m.content.starts_with("ERROR:") {
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

    fn update_escalation(&mut self, (signature, any_error): &(String, bool)) {
        let repeated = self.last_call_signature.as_deref() == Some(signature.as_str());
        if repeated || *any_error {
            self.repeat_count += 1;
        } else {
            self.repeat_count = 0;
        }
        self.last_call_signature = Some(signature.clone());

        if self.policy.auto_escalate
            && self.current_model == "hivemind"
            && self.repeat_count >= self.policy.escalate_after_repeats
        {
            self.ui.model_escalated(
                &self.current_model,
                &self.policy.escalate_to_model,
                "repeated or failing tool calls on this task",
            );
            self.current_model = self.policy.escalate_to_model.clone();
            self.repeat_count = 0;
        }
    }

    async fn compact_if_needed(&mut self) {
        let policy = CompactionPolicy {
            threshold_percent: self.policy.compaction_threshold_percent,
            keep_recent: COMPACTION_KEEP_RECENT,
        };
        // `last_total_tokens` is the previous *response's* real usage --
        // accurate, but stale by exactly one user message, since it can't
        // know about whatever was just appended for the turn about to be
        // sent. `estimate_tokens` is approximate but always current. Taking
        // the larger of the two never under-triggers relative to either
        // signal, which matters more here than being precise: this is what
        // catches a huge single paste on what would otherwise look like an
        // early, low-usage turn.
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

    /// Fold a compaction pass's own summarization-call cost into
    /// `session_cost_usd` -- without this, every compaction is a real,
    /// billed model call that's invisible to both `/cost` and `--budget`
    /// enforcement, a leak in the exact accounting `budget_usd` exists to
    /// guarantee. Returns the cost added, for the UI to surface immediately.
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

/// Index to truncate a transcript to so it no longer ends with an assistant
/// message whose `tool_calls` never received all their results. `None` when
/// the transcript is already well-formed.
///
/// Pure and free-standing so the rule can be tested directly -- building a
/// whole `Agent` (client, registry, workspace, UI) to exercise a list
/// operation would test the scaffolding, not the rule.
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
}
