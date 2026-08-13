//! Context compaction: once usage crosses a threshold percent of the active
//! model's context window, fold everything except the system prompt and the
//! most recent few turns into one model-generated summary message.
//!
//! This is what lets a long session keep running on the cheap Flash tier
//! instead of either failing outright at the context limit or silently
//! re-sending (and re-billing) an ever-growing transcript.

use harness_provider::{DeepSeekClient, ProviderError};
use harness_types::{ChatRequest, Message, Role, StreamEvent, Usage};

pub struct CompactionPolicy {
    pub threshold_percent: u8,
    /// Most-recent messages (after the system prompt) kept verbatim.
    pub keep_recent: usize,
}

pub struct CompactionReport {
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: u64,
    /// Usage from the summarization call itself -- this is a real sampled
    /// request against `summarizer_model` and costs real money, distinct
    /// from (and previously invisible next to) the main conversation's
    /// turn-by-turn usage. `None` only when the call failed outright (the
    /// fallback placeholder summary was used), not when the cost was zero.
    pub summary_usage: Option<Usage>,
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
    let keep_from = compacted_tail_start(messages, policy.keep_recent);

    let old: Vec<Message> = messages.drain(1..keep_from).collect();
    if old.is_empty() {
        return None;
    }

    let transcript = render_for_summary(&old);
    let (summary, summary_usage) = match summarize(summarizer, summarizer_model, &transcript).await
    {
        Ok((summary, usage)) => (summary, Some(usage)),
        Err(_) => (
            "(summary unavailable — a summarization call failed; earlier turns were dropped to free context space)".to_string(),
            None,
        ),
    };

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
        summary_usage,
    })
}


fn compacted_tail_start(messages: &[Message], keep_recent: usize) -> usize {
    // Clamp into `[1, len-1]` so the system prompt at [0] is always kept and
    // the index below can't run off the end (defensive against keep_recent 0).
    let mut start = messages
        .len()
        .saturating_sub(keep_recent)
        .clamp(1, messages.len().saturating_sub(1));
    while start > 1 && messages[start].role == Role::Tool {
        start -= 1;
    }
    start
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

/// Keeps both ends of `s`, not just the head -- a head-only truncate
/// systematically loses whatever comes later (a file path mentioned near
/// the end of a long tool result, a conclusion after a long chain of
/// reasoning), and the summarizer prompt explicitly asks it to preserve
/// file paths, which this alone can't guarantee but at least stops
/// reliably discarding.
fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    let head_len = max_chars * 2 / 3;
    let tail_len = max_chars - head_len;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}…{tail}")
}

async fn summarize(
    client: &DeepSeekClient,
    model: &str,
    transcript: &str,
) -> Result<(String, Usage), ProviderError> {
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
        // A summarization call builds a throwaway two-message prompt with
        // no tools and no shared prefix -- there is nothing here a cache
        // could ever hit, so a breakpoint would only add wire noise.
        cache_prompt_prefix: false,
    };
    let mut rx = client.stream(&req);
    let mut result = None;
    while let Some(event) = rx.recv().await {
        if let StreamEvent::Done(resp) = event? {
            result = Some((resp.content, resp.usage));
        }
    }
    result.ok_or(ProviderError::IncompleteStream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_types::ToolCall;

    fn sys() -> Message {
        Message::system("system")
    }
    fn user() -> Message {
        Message::user("hi")
    }
    fn asst_text() -> Message {
        Message::assistant("done")
    }
    fn asst_call() -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read_file".into(),
                args: serde_json::value::RawValue::from_string("{}".into()).unwrap(),
            }],
            tool_call_id: None,
            name: None,
        }
    }
    fn tool_res() -> Message {
        Message::tool_result("c", "read_file", "ok")
    }

    /// The API invariant compaction must never break: every `tool` message is
    /// immediately preceded either by another `tool` message (same response
    /// group) or by an assistant message that actually made tool calls.
    fn tool_pairing_is_valid(messages: &[Message]) -> bool {
        messages.iter().enumerate().all(|(i, m)| {
            if m.role != Role::Tool {
                return true;
            }
            match i.checked_sub(1).map(|j| &messages[j]) {
                Some(prev) => {
                    prev.role == Role::Tool
                        || (prev.role == Role::Assistant && !prev.tool_calls.is_empty())
                }
                None => false, // a tool message at index 0 is always orphaned
            }
        })
    }

    #[test]
    fn snaps_back_over_a_single_orphaned_tool_result() {
        // Boundary (len 13 - keep 8 = 5) lands on a tool result whose
        // assistant parent is at 4 — snapping to 4 keeps them together.
        let m = vec![
            sys(),
            user(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_text(),
        ];
        assert_eq!(compacted_tail_start(&m, 8), 4);
        assert_eq!(m[4].role, Role::Assistant);
    }

    #[test]
    fn snaps_back_over_a_whole_tool_run() {
        // One assistant turn with three parallel tool calls → three tool
        // results in a row (indices 3,4,5). Boundary at 5 must snap all the
        // way back to the assistant at 2, not stop mid-run.
        let m = vec![
            sys(),
            user(),
            asst_call(),
            tool_res(),
            tool_res(),
            tool_res(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
        ];
        assert_eq!(compacted_tail_start(&m, 8), 2);
        assert_eq!(m[2].role, Role::Assistant);
    }

    #[test]
    fn leaves_boundary_untouched_when_tail_starts_on_a_non_tool_message() {
        let m = vec![
            sys(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
            user(),
            asst_text(),
        ];
        // Nothing to snap: len 13 - keep 8 = 5, and m[5] is not a tool message.
        assert_eq!(compacted_tail_start(&m, 8), 5);
        assert_ne!(m[5].role, Role::Tool);
    }

    #[test]
    fn the_naive_boundary_would_have_orphaned_a_tool_message() {
        // Documents *why* the snap exists: replicate maybe_compact's slice
        // (drain 1..keep_from, insert a summary at 1) using the OLD boundary
        // and show it produces an invalid transcript.
        let mut m = vec![
            sys(),
            user(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_text(),
        ];
        let naive_keep_from = (m.len() - 8).max(1); // == 5, lands on a tool result
        m.drain(1..naive_keep_from);
        m.insert(1, Message::user("<summary>"));
        assert!(
            !tool_pairing_is_valid(&m),
            "the naive boundary should leave an orphaned tool message"
        );
    }

    #[test]
    fn slicing_at_the_snapped_boundary_leaves_a_valid_transcript() {
        // Same scenario, now sliced at the snapped boundary exactly as
        // maybe_compact does — the result must satisfy the API pairing rule.
        let mut m = vec![
            sys(),
            user(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_call(),
            tool_res(),
            asst_text(),
        ];
        let keep_from = compacted_tail_start(&m, 8);
        m.drain(1..keep_from);
        m.insert(1, Message::user("<summary>"));
        assert!(
            tool_pairing_is_valid(&m),
            "compaction must never orphan a tool message: {:?}",
            m.iter().map(|x| x.role).collect::<Vec<_>>()
        );
    }
}
