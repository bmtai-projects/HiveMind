use harness_types::Message;
const CHARS_PER_TOKEN: f64 = 3.3;
const PER_MESSAGE_OVERHEAD_TOKENS: u64 = 4;
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

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
