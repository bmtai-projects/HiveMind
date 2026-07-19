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
    // Note: reasoning effort is deliberately NOT sent — DeepSeek has no
    // confirmed `reasoning_effort` parameter; v4-pro reasons unconditionally
    // and streams it back as `reasoning_content`. Sending an unverified
    // field risks a hard 400 on strict deployments.
}

#[derive(Serialize)]
pub(crate) struct WireStreamOptions {
    pub include_usage: bool,
}

#[derive(Serialize)]
pub(crate) struct WireMessage {
    pub role: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content: String,
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

pub(crate) fn to_wire_messages(msgs: &[Message]) -> Vec<WireMessage> {
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
            WireMessage {
                role,
                content: m.content.clone(),
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
    /// DeepSeek context-caching fields — present only on cache-aware deployments.
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u64>,
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
