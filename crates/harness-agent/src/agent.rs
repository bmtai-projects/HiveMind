//! The core sample↔tools loop: stream a model, execute what it asks for in
//! parallel, feed results back, repeat — with compaction and Flash→Pro
//! escalation woven in as turn-boundary policy, not special cases.

use std::sync::Arc;

use harness_config::{AgentPolicy, ModelInfo, Resolved, Tier};
use harness_provider::DeepSeekClient;
use harness_tools::Registry;
use harness_types::{ChatRequest, Message, Role, StreamEvent, ToolCall};

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
}

impl Agent {
    pub fn new(
        resolved: Resolved,
        tools: Registry,
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
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    pub fn current_tier(&self) -> Tier {
        self.current_tier
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
                return Ok(());
            }

            self.dispatch_and_record(calls_for_dispatch).await;
            let signature = self.messages_tail_signature();
            self.update_escalation(&signature);
        }

        anyhow::bail!(
            "reached max turns ({}) without completing",
            self.policy.max_turns
        )
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
    /// each result to history in the calls' original order.
    async fn dispatch_and_record(&mut self, calls: Vec<ToolCall>) {
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
