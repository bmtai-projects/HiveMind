//! Provider-neutral wire model — the one internal shape every backend
//! normalizes into and out of. Analogue of grok-build's
//! `xai-grok-sampling-types`: the agent loop and tools only ever see these
//! types, never a vendor's dialect.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Role of a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single tool invocation requested by the model. `Clone` is cheap: it's
/// two small `String`s and a boxed `RawValue` (a boxed `str` under the
/// hood), not a deep JSON tree walk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments, deserialized lazily by the tool that handles them.
    pub args: Box<RawValue>,
}

/// One turn in the conversation, normalized across providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// Visible chain-of-thought, when a model streams `reasoning_content`
    /// (e.g. deepseek-v4-pro). Never sent back upstream.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            ..Self::empty(Role::System)
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            ..Self::empty(Role::User)
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            ..Self::empty(Role::Assistant)
        }
    }
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            content: content.into(),
            ..Self::empty(Role::Tool)
        }
    }
    fn empty(role: Role) -> Self {
        Self {
            role,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }
}

/// Model-facing description of a callable tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the arguments.
    pub parameters: serde_json::Value,
}

/// Token accounting for a completed response. `cache_hit_tokens` /
/// `cache_miss_tokens` come from DeepSeek's context-caching usage fields
/// (`prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`) — the whole
/// point of keeping the prompt prefix stable. `None` when a provider
/// doesn't report them.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cache_hit_tokens: Option<u64>,
    pub cache_miss_tokens: Option<u64>,
}

impl Usage {
    /// Fraction of prompt tokens served from cache, when known.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let hit = self.cache_hit_tokens?;
        let miss = self.cache_miss_tokens.unwrap_or(0);
        let denom = hit + miss;
        (denom > 0).then(|| hit as f64 / denom as f64)
    }
}

/// A single sampling request, provider-neutral.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Passthrough for models that support OpenRouter's `reasoning_effort`
    /// parameter -- deliberately a free string, not a closed enum: valid
    /// values genuinely differ per model (e.g. "hivemind" only accepts
    /// "high"/"xhigh"; others add "xhigh"/"max"/"none"), so the real
    /// validation lives per-model in `harness_config::ModelCatalogEntry`,
    /// not in this type. The caller is expected to have already checked
    /// the active model actually supports whatever value is set here.
    pub reasoning_effort: Option<String>,
    /// Ask the provider to cache the prompt prefix (system prompt + tool
    /// manifest) by marking it with an explicit breakpoint on the wire.
    ///
    /// Same contract as `reasoning_effort` above: purely a passthrough that
    /// the *caller* is responsible for gating, because only some providers
    /// accept it (see `harness_config::ModelCatalogEntry::
    /// needs_explicit_cache_control`). Providers that cache automatically
    /// need this off — sending it to them buys nothing and risks a 400.
    pub cache_prompt_prefix: bool,
}

/// The assembled result of one sampling request.
#[derive(Debug, Clone, Default)]
pub struct ChatResponse {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: String,
    pub model: String,
}

/// One incremental update from a streaming sampling request. Exactly one
/// terminal event ([`StreamEvent::Done`]) ends the stream.
#[derive(Debug)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    Done(Box<ChatResponse>),
}
