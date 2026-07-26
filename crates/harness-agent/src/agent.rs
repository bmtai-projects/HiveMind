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
use crate::ui::Ui;

/// Most-recent messages (after the system prompt) a compaction pass keeps
/// verbatim. Not user-configurable yet — a reasonable fixed default.
const COMPACTION_KEEP_RECENT: usize = 8;

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
            last_total_tokens: 0,
            context_window,
            repeat_count: 0,
            last_call_signature: None,
            workspace,
            checkpoints: Vec::new(),
            hooks: resolved.hooks,
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
            &self.current_model,
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
            self.compact_if_needed().await;

            // Gated fresh every turn, not just at set_reasoning_effort time
            // -- /model can switch to something that doesn't support
            // whatever was requested (or doesn't support the parameter at
            // all), and this must silently omit it rather than send a
            // value the active model will 400 on.
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
            };

            let mut rx = self.client.stream(&req);
            // `stream()` only borrowed `req` synchronously to serialize the
            // wire request; ownership is ours again immediately, so we
            // never had to clone the conversation just to send it.
            self.messages = std::mem::take(&mut req.messages);

            let resp = self.drain_stream(&mut rx).await?;

            self.last_total_tokens = resp.usage.total_tokens;
            self.context_window = harness_config::lookup_model(&self.current_model)
                .map(|m| m.context_window)
                .unwrap_or(self.context_window);
            self.ui.usage(&resp.usage, &self.current_model, self.hosted);

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
        if let Some(report) = maybe_compact(
            &mut self.messages,
            self.last_total_tokens,
            self.context_window,
            &policy,
            &self.client,
            &self.current_model,
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
