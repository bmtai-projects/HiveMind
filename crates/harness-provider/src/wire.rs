//! DeepSeek's wire dialect (OpenAI Chat Completions-compatible). Kept
//! separate from [`crate::client`] so the request/response JSON shape is
//! easy to audit against DeepSeek's docs independent of transport/retry
//! logic.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use harness_types::{Message, Role, ToolCall, ToolSchema};

#[derive(Serialize)]
pub(crate) struct WireRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    pub stream: bool,
    pub stream_options: WireStreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    // Sent only when the caller has already confirmed the active model
    // supports this exact value (see harness_config::ModelCatalogEntry's
    // `reasoning_efforts` and harness_agent::Agent's per-turn gating) --
    // never sent speculatively, since an unsupported value is a hard 400
    // on some deployments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'a str>,
}

#[derive(Serialize)]
pub(crate) struct WireStreamOptions {
    pub include_usage: bool,
}

/// Message content on the wire.
///
/// Normally a plain string. It becomes an array of parts only when a
/// cache breakpoint has to ride along, because `cache_control` attaches to
/// a *content part*, not to the message. Untagged so each variant
/// serializes as its natural JSON shape, keeping the common case
/// byte-identical to what every provider has always received.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum WireContent {
    Text(String),
    Parts(Vec<WireContentPart>),
}

#[derive(Serialize)]
pub(crate) struct WireContentPart {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<WireCacheControl>,
}

#[derive(Serialize)]
pub(crate) struct WireCacheControl {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[derive(Serialize)]
pub(crate) struct WireMessage {
    pub role: &'static str,
    /// `None` -- not `Some("")` -- for a tool-call-only assistant message.
    /// The distinction is load-bearing: emitting an empty string here once
    /// tripped a hosted-backend schema that only accepted a present string,
    /// so the omission is preserved deliberately.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<WireContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct WireToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireFunctionCall,
}

#[derive(Serialize)]
pub(crate) struct WireFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
pub(crate) struct WireTool {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireToolSchema,
}

#[derive(Serialize)]
pub(crate) struct WireToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Build the wire messages.
///
/// When `cache_prompt_prefix` is set, the **system** message is emitted as
/// a single content part carrying a `cache_control` breakpoint. Anthropic
/// caches everything up to and including the marked part -- and since the
/// tool manifest is serialized ahead of the messages, one breakpoint there
/// covers the entire fixed prefix (tools + system prompt), which is the
/// part that never changes between turns and therefore the only part worth
/// caching. Every other message stays a plain string.
pub(crate) fn to_wire_messages(msgs: &[Message], cache_prompt_prefix: bool) -> Vec<WireMessage> {
    msgs.iter()
        .map(|m| {
            let role = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let tool_calls = if m.role == Role::Assistant {
                m.tool_calls
                    .iter()
                    .map(|tc| WireToolCall {
                        id: tc.id.clone(),
                        kind: "function",
                        function: WireFunctionCall {
                            name: tc.name.clone(),
                            arguments: tc.args.get().to_string(),
                        },
                    })
                    .collect()
            } else {
                Vec::new()
            };
            // Empty stays absent, never `""` -- see WireMessage::content.
            let content = if m.content.is_empty() {
                None
            } else if cache_prompt_prefix && m.role == Role::System {
                Some(WireContent::Parts(vec![WireContentPart {
                    kind: "text",
                    text: m.content.clone(),
                    cache_control: Some(WireCacheControl { kind: "ephemeral" }),
                }]))
            } else {
                Some(WireContent::Text(m.content.clone()))
            };

            WireMessage {
                role,
                content,
                tool_calls,
                tool_call_id: (m.role == Role::Tool)
                    .then(|| m.tool_call_id.clone())
                    .flatten(),
                name: (m.role == Role::Tool).then(|| m.name.clone()).flatten(),
            }
        })
        .collect()
}

pub(crate) fn to_wire_tools(tools: &[ToolSchema]) -> Vec<WireTool> {
    tools
        .iter()
        .map(|t| WireTool {
            kind: "function",
            function: WireToolSchema {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect()
}

// ---- streamed response shapes ----

#[derive(Deserialize)]
pub(crate) struct WireStreamChunk {
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Deserialize)]
pub(crate) struct WireChoice {
    #[serde(default)]
    pub delta: WireDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
pub(crate) struct WireDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<WireToolCallDelta>,
}

#[derive(Deserialize)]
pub(crate) struct WireToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<WireFunctionDelta>,
}

#[derive(Default, Deserialize)]
pub(crate) struct WireFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// DeepSeek's *native* context-caching fields, sent when talking to
    /// DeepSeek's own API directly (the hosted backend's fallback path).
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u64>,
    /// The OpenAI-dialect spelling of the same thing, which is what
    /// OpenRouter normalizes every provider's cache reporting into -- i.e.
    /// what hosted HiveMind traffic actually carries. Without this, cache
    /// hits were invisible to the CLI on the entire hosted path: the cost
    /// readout never showed a cache %, and `estimate_cost_usd` billed
    /// every cached token at the full uncached rate.
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
}

#[derive(Deserialize)]
pub(crate) struct WirePromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

impl WireUsage {
    /// Prompt tokens served from cache, from whichever dialect reported it.
    /// DeepSeek's native field wins when both are present -- same
    /// precedence the hosted backend's own billing uses
    /// (`HiveMind-server/src/proxy/cost.ts`), so the client-side estimate
    /// and the server-side charge can't disagree about what was cached.
    pub fn cache_hit_tokens(&self) -> Option<u64> {
        self.prompt_cache_hit_tokens
            .or_else(|| self.prompt_tokens_details.as_ref()?.cached_tokens)
    }

    /// Prompt tokens that were *not* cached. Derived from the hit count
    /// when only that was reported. `saturating_sub` because a provider
    /// reporting `cached_tokens > prompt_tokens` (malformed, but not worth
    /// crashing over) must not underflow into a nonsense huge number.
    pub fn cache_miss_tokens(&self) -> Option<u64> {
        self.prompt_cache_miss_tokens
            .or_else(|| Some(self.prompt_tokens.saturating_sub(self.cache_hit_tokens()?)))
    }
}

/// Turn accumulated raw JSON-fragment text for one tool call's arguments
/// into a validated `RawValue`, falling back to `{}` if the stream was cut
/// mid-argument (recoverable: the tool call will simply fail cleanly when
/// dispatched with empty args, rather than panicking the decoder).
pub(crate) fn finalize_args(raw: &str) -> Box<RawValue> {
    let candidate = if raw.trim().is_empty() { "{}" } else { raw };
    RawValue::from_string(candidate.to_string())
        .unwrap_or_else(|_| RawValue::from_string("{}".to_string()).expect("literal is valid JSON"))
}

pub(crate) fn tool_call(id: String, name: String, args_raw: &str) -> ToolCall {
    ToolCall {
        id,
        name,
        args: finalize_args(args_raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(json: &str) -> WireUsage {
        serde_json::from_str(json).expect("fixture is valid JSON")
    }

    fn to_json(msgs: &[Message], cache: bool) -> serde_json::Value {
        serde_json::to_value(to_wire_messages(msgs, cache)).unwrap()
    }

    #[test]
    fn without_caching_content_is_a_plain_string() {
        // The shape every provider has always received; must stay
        // byte-identical for the models that cache automatically.
        let j = to_json(&[Message::system("you are an agent")], false);
        assert_eq!(j[0]["content"], serde_json::json!("you are an agent"));
    }

    #[test]
    fn with_caching_the_system_prompt_carries_a_breakpoint() {
        let j = to_json(&[Message::system("you are an agent")], true);
        assert_eq!(j[0]["content"][0]["type"], "text");
        assert_eq!(j[0]["content"][0]["text"], "you are an agent");
        assert_eq!(j[0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn only_the_system_message_gets_a_breakpoint() {
        // Anthropic allows a limited number of breakpoints, and only the
        // fixed prefix is worth one -- marking user turns would spend them
        // on content that changes every request.
        let j = to_json(
            &[
                Message::system("sys"),
                Message::user("hello"),
                Message::assistant("hi"),
            ],
            true,
        );
        assert!(j[0]["content"].is_array(), "system should be parts");
        assert_eq!(j[1]["content"], serde_json::json!("hello"));
        assert_eq!(j[2]["content"], serde_json::json!("hi"));
    }

    #[test]
    fn a_tool_call_only_message_still_omits_content_entirely() {
        // Load-bearing: emitting `""` here (rather than omitting the key)
        // is what once produced a hard 400 from the hosted backend. Must
        // hold in BOTH modes.
        let assistant_with_calls = Message {
            role: Role::Assistant,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "list_dir".into(),
                args: RawValue::from_string("{}".into()).unwrap(),
            }],
            tool_call_id: None,
            name: None,
        };
        for cache in [false, true] {
            let j = to_json(std::slice::from_ref(&assistant_with_calls), cache);
            assert!(
                j[0].get("content").is_none(),
                "content must be absent, not empty (cache={cache}): {j}"
            );
            assert_eq!(j[0]["tool_calls"][0]["function"]["name"], "list_dir");
        }
    }

    #[test]
    fn tool_results_keep_their_pairing_fields_in_both_modes() {
        let msgs = [Message::tool_result("c1", "list_dir", "a.txt")];
        for cache in [false, true] {
            let j = to_json(&msgs, cache);
            assert_eq!(j[0]["role"], "tool");
            assert_eq!(j[0]["tool_call_id"], "c1");
            assert_eq!(j[0]["name"], "list_dir");
            assert_eq!(j[0]["content"], serde_json::json!("a.txt"));
        }
    }

    #[test]
    fn reads_openrouters_openai_dialect_cache_field() {
        // The shape hosted HiveMind traffic actually carries.
        let u = usage(
            r#"{"prompt_tokens":1000,"completion_tokens":10,"total_tokens":1010,
                "prompt_tokens_details":{"cached_tokens":800}}"#,
        );
        assert_eq!(u.cache_hit_tokens(), Some(800));
        assert_eq!(u.cache_miss_tokens(), Some(200));
    }

    #[test]
    fn still_reads_deepseeks_native_fields() {
        let u = usage(
            r#"{"prompt_tokens":1000,"completion_tokens":10,"total_tokens":1010,
                "prompt_cache_hit_tokens":700,"prompt_cache_miss_tokens":300}"#,
        );
        assert_eq!(u.cache_hit_tokens(), Some(700));
        assert_eq!(u.cache_miss_tokens(), Some(300));
    }

    #[test]
    fn native_fields_win_when_both_dialects_are_present() {
        // Matches the hosted backend's own precedence so the client-side
        // estimate and the server-side charge agree.
        let u = usage(
            r#"{"prompt_tokens":1000,"completion_tokens":10,"total_tokens":1010,
                "prompt_cache_hit_tokens":700,"prompt_cache_miss_tokens":300,
                "prompt_tokens_details":{"cached_tokens":800}}"#,
        );
        assert_eq!(u.cache_hit_tokens(), Some(700));
        assert_eq!(u.cache_miss_tokens(), Some(300));
    }

    #[test]
    fn no_cache_reporting_at_all_stays_unknown_not_zero() {
        // `None` is load-bearing: it means "this provider didn't say",
        // which the UI renders as no cache note at all. Reporting Some(0)
        // here would claim a confirmed 0% hit rate we never measured.
        let u = usage(r#"{"prompt_tokens":1000,"completion_tokens":10,"total_tokens":1010}"#);
        assert_eq!(u.cache_hit_tokens(), None);
        assert_eq!(u.cache_miss_tokens(), None);
    }

    #[test]
    fn an_explicit_zero_cached_tokens_is_a_real_measured_zero() {
        // Distinct from the case above: the provider *did* report, and the
        // answer was "nothing was cached" -- worth showing as cache 0%.
        let u = usage(
            r#"{"prompt_tokens":1000,"completion_tokens":10,"total_tokens":1010,
                "prompt_tokens_details":{"cached_tokens":0}}"#,
        );
        assert_eq!(u.cache_hit_tokens(), Some(0));
        assert_eq!(u.cache_miss_tokens(), Some(1000));
    }

    #[test]
    fn cached_exceeding_prompt_tokens_saturates_instead_of_underflowing() {
        // Malformed upstream data must not wrap around into a huge number.
        let u = usage(
            r#"{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110,
                "prompt_tokens_details":{"cached_tokens":500}}"#,
        );
        assert_eq!(u.cache_hit_tokens(), Some(500));
        assert_eq!(u.cache_miss_tokens(), Some(0));
    }

    #[test]
    fn a_details_object_with_no_cached_tokens_key_is_still_unknown() {
        let u = usage(
            r#"{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110,
                "prompt_tokens_details":{}}"#,
        );
        assert_eq!(u.cache_hit_tokens(), None);
        assert_eq!(u.cache_miss_tokens(), None);
    }
}
