//! Approximate token counting for an outgoing request, computed *before*
//! sending it. A real BPE tokenizer would need one implementation per
//! upstream provider -- DeepSeek, Anthropic, OpenAI, Google, xAI, Qwen, and
//! Moonshot all tokenize differently, and HiveMind brokers all seven behind
//! one CLI -- so exact counting isn't a "pick one library" problem, it's a
//! "ship and maintain seven" problem for a number that's only ever used as
//! a trigger, not billed on. A character-based estimate is honest about
//! being approximate and deliberately biased to over-count: triggering
//! compaction a turn earlier than strictly necessary costs a bit of
//! history; failing to trigger it is a hard failure from the provider
//! mid-task, with no history left to fall back on.

use harness_types::Message;

/// Rough chars-per-token ratio. English prose runs closer to 4; code and
/// JSON (tool arguments, dense symbols) run closer to 3. Biased toward the
/// code end on purpose -- a coding agent's transcript is mostly file
/// contents and tool calls, and over-counting is the safe direction here.
const CHARS_PER_TOKEN: f64 = 3.3;

/// Most chat-completions APIs bill a handful of tokens per message beyond
/// its raw content (role marker, separators). Ignoring this under-counts
/// worse the more messages there are -- exactly when the estimate matters
/// most.
const PER_MESSAGE_OVERHEAD_TOKENS: u64 = 4;

/// Estimate the token count of `messages` as they'd actually go out on the
/// wire: message content, plus every tool call's name and raw argument
/// string, plus tool-result bodies (already plain `content` on a `Tool`
/// role message). Always a deliberate over-estimate -- see module docs.
/// Used to trigger compaction *earlier* than waiting on the previous
/// turn's real `Usage` alone would, and as a last-resort pre-send guard;
/// never a substitute for a real provider-reported `Usage`.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

/// One message's share of the estimate above. Split out so `crate::trim` can
/// price an individual result before and after eliding it using the exact
/// same arithmetic the trigger thresholds are measured in -- two separate
/// formulas would let the saving it reports drift from the total it is
/// trying to bring down.
pub fn estimate_message_tokens(m: &Message) -> u64 {
    let mut chars = m.content.chars().count();
    for tc in &m.tool_calls {
        chars += tc.name.chars().count() + tc.args.get().chars().count();
    }
    (chars as f64 / CHARS_PER_TOKEN).ceil() as u64 + PER_MESSAGE_OVERHEAD_TOKENS
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_types::{Role, ToolCall};

    #[test]
    fn empty_history_is_zero() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn counts_content_and_per_message_overhead() {
        let m = vec![Message::system("x".repeat(33))]; // 33 chars / 3.3 = 10
        assert_eq!(estimate_tokens(&m), 10 + PER_MESSAGE_OVERHEAD_TOKENS);
    }

    #[test]
    fn counts_tool_call_name_and_arguments_too() {
        let m = vec![Message {
            role: Role::Assistant,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read_file".into(), // 9 chars
                args: serde_json::value::RawValue::from_string(
                    r#"{"path":"src/main.rs"}"#.into(), // 23 chars
                )
                .unwrap(),
            }],
            tool_call_id: None,
            name: None,
        }];
        let expected =
            ((9 + 23) as f64 / CHARS_PER_TOKEN).ceil() as u64 + PER_MESSAGE_OVERHEAD_TOKENS;
        assert_eq!(estimate_tokens(&m), expected);
    }

    #[test]
    fn a_huge_single_message_dominates_the_total() {
        let small = vec![Message::system("hi"), Message::user("hi")];
        let mut with_huge_paste = small.clone();
        with_huge_paste.push(Message::user("x".repeat(1_000_000)));
        assert!(estimate_tokens(&with_huge_paste) > estimate_tokens(&small) + 200_000);
    }
}
