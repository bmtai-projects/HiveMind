//! Context compaction: once usage crosses a threshold percent of the active
//! model's context window, fold everything except the system prompt and the
//! most recent few turns into one model-generated summary message.
//!
//! This is what lets a long session keep running on the cheap Flash tier
//! instead of either failing outright at the context limit or silently
//! re-sending (and re-billing) an ever-growing transcript.

use harness_provider::{DeepSeekClient, ProviderError};
use harness_types::{ChatRequest, Message, Role, StreamEvent};

pub struct CompactionPolicy {
    pub threshold_percent: u8,
    /// Most-recent messages (after the system prompt) kept verbatim.
    pub keep_recent: usize,
}

pub struct CompactionReport {
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: u64,
}

/// Compact `messages` in place if `total_tokens` has crossed the policy
/// threshold relative to `context_window`. Returns `None` when nothing was
/// done (below threshold, or too little history to bother compacting).
///
/// `messages[0]` must be the system prompt — an invariant `Agent` upholds
/// and this function never violates (it only ever drains `1..`).
pub async fn maybe_compact(
    messages: &mut Vec<Message>,
    total_tokens: u64,
    context_window: u64,
    policy: &CompactionPolicy,
    summarizer: &DeepSeekClient,
    summarizer_model: &str,
) -> Option<CompactionReport> {
    if context_window == 0 || total_tokens == 0 {
        return None;
    }
    let percent_used = total_tokens.saturating_mul(100) / context_window;
    if percent_used < policy.threshold_percent as u64 {
        return None;
    }
    if messages.len() <= policy.keep_recent + 1 {
        return None; // not enough history to be worth summarizing
    }

    let messages_before = messages.len();
    let keep_from = (messages.len() - policy.keep_recent).max(1);

    let old: Vec<Message> = messages.drain(1..keep_from).collect();
    if old.is_empty() {
        return None;
    }

    let transcript = render_for_summary(&old);
    let summary = summarize(summarizer, summarizer_model, &transcript)
        .await
        .unwrap_or_else(|_| "(summary unavailable — a summarization call failed; earlier turns were dropped to free context space)".to_string());

    messages.insert(
        1,
        Message::user(format!(
            "<earlier-context-summary>\n{summary}\n</earlier-context-summary>"
        )),
    );

    Some(CompactionReport {
        messages_before,
        messages_after: messages.len(),
        tokens_before: total_tokens,
    })
}

fn render_for_summary(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        let tag = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        if !m.content.is_empty() {
            out.push_str(&format!("[{tag}] {}\n", truncate(&m.content, 800)));
        }
        for tc in &m.tool_calls {
            out.push_str(&format!(
                "[{tag} called tool] {}({})\n",
                tc.name,
                truncate(tc.args.get(), 200)
            ));
        }
    }
    out
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}…")
}

async fn summarize(
    client: &DeepSeekClient,
    model: &str,
    transcript: &str,
) -> Result<String, ProviderError> {
    let req = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message::system(
                "You compress coding-agent conversation history. Summarize the transcript below \
                 concisely: preserve file paths touched, key decisions, and unresolved tasks. \
                 Output only the summary, no preamble."
                    .to_string(),
            ),
            Message::user(transcript.to_string()),
        ],
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(600),
        reasoning_effort: None,
    };
    let mut rx = client.stream(&req);
    let mut result = None;
    while let Some(event) = rx.recv().await {
        if let StreamEvent::Done(resp) = event? {
            result = Some(resp.content);
        }
    }
    result.ok_or(ProviderError::IncompleteStream)
}
