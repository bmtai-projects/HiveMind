//! The core sample↔tools loop: stream a model, execute what it asks for in
//! parallel, feed results back, repeat — with compaction and Flash→Pro
//! escalation woven in as turn-boundary policy, not special cases.

use std::sync::Arc;

use harness_config::{AgentPolicy, ModelInfo, Resolved, Tier};
use harness_provider::DeepSeekClient;
use harness_tools::{Registry, Workspace};
use harness_types::{ChatRequest, Message, Role, StreamEvent, ToolCall};

use crate::checkpoint::{self, Checkpoint, UndoReport};
use crate::compaction::{CompactionPolicy, maybe_compact};
use crate::ui::Ui;

/// Most-recent messages (after the system prompt) a compaction pass keeps
/// verbatim. Not user-configurable yet — a reasonable fixed default.
const COMPACTION_KEEP_RECENT: usize = 8;

pub struct Agent {
    client: DeepSeekClient,
    tools: Registry,
    flash: ModelInfo,
    pro: ModelInfo,
    policy: AgentPolicy,
    ui: Arc<dyn Ui>,

    messages: Vec<Message>,
    current_tier: Tier,
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

        let current_tier = resolved.policy.default_tier;
        let context_window = resolved.model_for(current_tier).context_window;

        Self {
            client,
            tools,
            flash: resolved.flash,
            pro: resolved.pro,
            policy: resolved.policy,
            ui,
            messages: vec![Message::system(system_prompt)],
            current_tier,
            last_total_tokens: 0,
            context_window,
            repeat_count: 0,
            last_call_signature: None,
            workspace,
            checkpoints: Vec::new(),
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    pub fn current_tier(&self) -> Tier {
        self.current_tier
    }

    /// Set the tier this session runs on, effective immediately and sticky
    /// across future inputs (unlike auto-escalation, which resets to the
    /// configured default at the start of every new `run()` call). Used by
    /// an explicit user command (e.g. a REPL `/tier` command), not by the
    /// doom-loop guard.
    pub fn set_default_tier(&mut self, tier: Tier) {
        self.policy.default_tier = tier;
        self.current_tier = tier;
    }

    /// Force a compaction pass right now, bypassing the usage-threshold
    /// check (an explicit user command, not the automatic turn-boundary
    /// check `run()` already does). Returns `true` if there was enough
    /// history to actually compact.
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
            &self.flash.wire_id,
        )
        .await
        {
            self.ui.compacted(
                report.messages_before,
                report.messages_after,
                report.tokens_before,
            );
            self.last_total_tokens = 0;
            true
        } else {
            false
        }
    }

    fn model_info_for(&self, tier: Tier) -> &ModelInfo {
        match tier {
            Tier::Flash => &self.flash,
            Tier::Pro => &self.pro,
        }
    }

    /// Process one user input to completion, streaming to `self.ui`.
    /// Escalation is scoped to a single call: tier resets to the configured
    /// default at the start of every new input, so a hard task pays for
    /// Pro only while it needs it.
    pub async fn run(&mut self, user_input: &str) -> anyhow::Result<()> {
        self.current_tier = self.policy.default_tier;
        self.repeat_count = 0;
        self.last_call_signature = None;
        let mut checkpoint = Checkpoint::open(user_input, self.messages.len());
        self.messages.push(Message::user(user_input.to_string()));

        for _turn in 0..self.policy.max_turns {
            self.compact_if_needed().await;

            let model_info = self.model_info_for(self.current_tier).clone();
            let mut req = ChatRequest {
                model: model_info.wire_id.clone(),
                messages: std::mem::take(&mut self.messages),
                tools: self.tools.schemas(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
            };

            let mut rx = self.client.stream(&req);
            // `stream()` only borrowed `req` synchronously to serialize the
            // wire request; ownership is ours again immediately, so we
            // never had to clone the conversation just to send it.
            self.messages = std::mem::take(&mut req.messages);

            let resp = self.drain_stream(&mut rx).await?;

            self.last_total_tokens = resp.usage.total_tokens;
            self.context_window = model_info.context_window;
            self.ui.usage(&resp.usage, self.current_tier, &model_info);

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
                return Ok(());
            }

            self.dispatch_and_record(calls_for_dispatch, &mut checkpoint)
                .await;
            let signature = self.messages_tail_signature();
            self.update_escalation(&signature);
        }

        checkpoint::push(&mut self.checkpoints, checkpoint);
        anyhow::bail!(
            "reached max turns ({}) without completing",
            self.policy.max_turns
        )
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
    async fn dispatch_and_record(&mut self, calls: Vec<ToolCall>, checkpoint: &mut Checkpoint) {
        checkpoint.capture(&self.workspace, &calls).await;
        for call in &calls {
            self.ui.tool_start(&call.name, call.args.get());
        }
        let results = self.tools.dispatch_many(calls).await;
        for (call, result) in results {
            let is_error = result.starts_with("ERROR:");
            self.ui.tool_end(&call.name, &result, is_error);
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
            && self.current_tier == Tier::Flash
            && self.repeat_count >= self.policy.escalate_after_repeats
        {
            self.ui.tier_escalated(
                Tier::Flash,
                Tier::Pro,
                "repeated or failing tool calls on this task",
            );
            self.current_tier = Tier::Pro;
            self.repeat_count = 0;
        }
    }

    async fn compact_if_needed(&mut self) {
        let policy = CompactionPolicy {
            threshold_percent: self.policy.compaction_threshold_percent,
            keep_recent: COMPACTION_KEEP_RECENT,
        };
        if let Some(report) = maybe_compact(
            &mut self.messages,
            self.last_total_tokens,
            self.context_window,
            &policy,
            &self.client,
            &self.flash.wire_id,
        )
        .await
        {
            self.ui.compacted(
                report.messages_before,
                report.messages_after,
                report.tokens_before,
            );
            // The next request's usage will reflect the smaller prompt;
            // reset our tracked total so we don't immediately re-trigger.
            self.last_total_tokens = 0;
        }
    }
}
