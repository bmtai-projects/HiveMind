//! Anthropic's native Messages API dialect — a genuinely different wire
//! shape from [`crate::client::ChatClient`]'s OpenAI Chat Completions
//! dialect: `/v1/messages` instead of `/chat/completions`, `x-api-key`
//! instead of `Authorization: Bearer`, system prompt as its own top-level
//! field instead of a message, tool calls as `tool_use` content blocks
//! instead of `tool_calls`, and a named-event SSE stream instead of one
//! flat `chat.completion.chunk` shape.
//!
//! Translation happens entirely at this module's edge: every event this
//! client emits on its [`EventStream`] is the same [`StreamEvent`]/
//! [`ChatResponse`] shape [`crate::client::ChatClient`] produces, so nothing
//! above `harness-provider` needs to know which dialect actually served a
//! request.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedSender};

use harness_types::{ChatRequest, ChatResponse, Message, Role, StreamEvent, ToolCall, Usage};

use crate::client::{EventStream, RetryHook};
use crate::error::ProviderError;
use crate::retry::{DEFAULT_MAX_RETRIES, backoff_delay};
use crate::wire::finalize_args;

/// Anthropic rejects a request with no `max_tokens` at all -- unlike the
/// OpenAI dialect, where omitting it just means "no cap."
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;
/// Stable API version header. Anthropic revs this independently of model
/// versions; update if a future API change requires a newer one.
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Clone)]
pub struct AnthropicClient {
    http: Client,
    base_url: Arc<str>,
    api_key: Arc<str>,
    max_retries: u32,
    on_retry: Option<RetryHook>,
}

impl AnthropicClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(300))
            .pool_idle_timeout(Duration::from_secs(600))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .expect("reqwest client with default TLS backend should always build");
        Self {
            http,
            base_url: Arc::from(base_url.into().trim_end_matches('/').to_string().as_str()),
            api_key: Arc::from(api_key.into().as_str()),
            max_retries: DEFAULT_MAX_RETRIES,
            on_retry: None,
        }
    }

    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    pub fn with_retry_hook(mut self, hook: RetryHook) -> Self {
        self.on_retry = Some(hook);
        self
    }

    /// Open the TLS connection now so the first real request doesn't pay
    /// for it -- see `ChatClient::warm` for the full rationale, identical
    /// here.
    pub fn warm(&self) {
        let http = self.http.clone();
        let url = self.base_url.to_string();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                http.get(&url).header("Accept", "*/*").send(),
            )
            .await;
        });
    }

    pub fn stream(&self, req: &ChatRequest) -> EventStream {
        let (tx, rx) = mpsc::unbounded_channel();

        let body: Bytes = match build_request(req) {
            Ok(wire) => match serde_json::to_vec(&wire) {
                Ok(b) => Bytes::from(b),
                Err(e) => {
                    let _ = tx.send(Err(ProviderError::Request(e.to_string())));
                    return rx;
                }
            },
            Err(msg) => {
                let _ = tx.send(Err(ProviderError::Request(msg)));
                return rx;
            }
        };

        let client = self.clone();
        tokio::spawn(async move {
            client.run_with_retries(body, tx).await;
        });
        rx
    }

    async fn run_with_retries(
        &self,
        body: Bytes,
        tx: UnboundedSender<Result<StreamEvent, ProviderError>>,
    ) {
        let mut attempt = 0u32;
        loop {
            match self.try_once(body.clone(), &tx).await {
                Ok(()) => return,
                Err(e) if e.is_retryable() && attempt < self.max_retries => {
                    let delay = backoff_delay(attempt, e.retry_after_secs());
                    if let Some(hook) = &self.on_retry {
                        hook(attempt + 1, self.max_retries, delay, &e);
                    }
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    let final_error = if e.is_retryable() {
                        ProviderError::RetriesExhausted(attempt + 1, Box::new(e))
                    } else {
                        e
                    };
                    let _ = tx.send(Err(final_error));
                    return;
                }
            }
        }
    }

    async fn try_once(
        &self,
        body: Bytes,
        tx: &UnboundedSender<Result<StreamEvent, ProviderError>>,
    ) -> Result<(), ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("x-api-key", self.api_key.as_ref())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .body(body)
            .send()
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let body_text = resp.text().await.unwrap_or_default();
            let snippet: String = body_text.chars().take(2000).collect();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body: snippet,
                retry_after_secs,
            });
        }

        self.decode_stream(resp, tx).await
    }

    async fn decode_stream(
        &self,
        resp: reqwest::Response,
        tx: &UnboundedSender<Result<StreamEvent, ProviderError>>,
    ) -> Result<(), ProviderError> {
        let mut byte_stream = resp.bytes_stream();
        let mut buf = String::new();

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut stop_reason = String::new();
        let mut input_tokens = 0u64;
        let mut cache_creation_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut output_tokens = 0u64;

        struct Acc {
            id: String,
            name: String,
            args: String,
        }
        let mut tool_blocks: BTreeMap<usize, Acc> = BTreeMap::new();
        let mut saw_any_frame = false;

        while let Some(chunk) = byte_stream.next().await {
            let chunk: Bytes = chunk.map_err(|e| ProviderError::Request(e.to_string()))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..=pos);

                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }

                let Ok(event) = serde_json::from_str::<AnthropicEvent>(data) else {
                    continue; // tolerate keep-alives / frames we don't model
                };
                saw_any_frame = true;

                match event {
                    AnthropicEvent::MessageStart { message } => {
                        input_tokens = message.usage.input_tokens;
                        cache_creation_tokens = message.usage.cache_creation_input_tokens;
                        cache_read_tokens = message.usage.cache_read_input_tokens;
                    }
                    AnthropicEvent::ContentBlockStart {
                        index,
                        content_block,
                    } => match content_block {
                        ContentBlockStart::ToolUse { id, name } => {
                            let _ = tx.send(Ok(StreamEvent::ToolCallStarted(name.clone())));
                            tool_blocks.insert(
                                index,
                                Acc {
                                    id,
                                    name,
                                    args: String::new(),
                                },
                            );
                        }
                        ContentBlockStart::Text { .. } | ContentBlockStart::Thinking { .. } => {}
                    },
                    AnthropicEvent::ContentBlockDelta { index, delta } => match delta {
                        ContentDelta::TextDelta { text } => {
                            content.push_str(&text);
                            let _ = tx.send(Ok(StreamEvent::TextDelta(text)));
                        }
                        ContentDelta::ThinkingDelta { thinking } => {
                            reasoning.push_str(&thinking);
                            let _ = tx.send(Ok(StreamEvent::ReasoningDelta(thinking)));
                        }
                        ContentDelta::InputJsonDelta { partial_json } => {
                            if let Some(acc) = tool_blocks.get_mut(&index) {
                                acc.args.push_str(&partial_json);
                            }
                        }
                        ContentDelta::SignatureDelta { .. } => {}
                    },
                    AnthropicEvent::ContentBlockStop { .. } => {}
                    AnthropicEvent::MessageDelta { delta, usage } => {
                        if let Some(reason) = delta.stop_reason {
                            stop_reason = reason;
                        }
                        if let Some(u) = usage {
                            output_tokens = u.output_tokens;
                        }
                    }
                    AnthropicEvent::MessageStop {} => break,
                    // A rare mid-stream failure (e.g. "overloaded_error");
                    // treated as transient rather than terminal -- the
                    // retry loop above decides whether another attempt is
                    // worth making.
                    AnthropicEvent::Error { error } => {
                        return Err(ProviderError::Request(error.message));
                    }
                    AnthropicEvent::Ping {} => {}
                }
            }
        }

        if !saw_any_frame {
            return Err(ProviderError::IncompleteStream);
        }

        let tool_calls: Vec<ToolCall> = tool_blocks
            .into_values()
            .map(|acc| ToolCall {
                id: acc.id,
                name: acc.name,
                args: finalize_args(&acc.args),
            })
            .collect();

        let cache_hit = cache_read_tokens;
        let cache_miss = input_tokens + cache_creation_tokens;
        let usage = Usage {
            prompt_tokens: cache_hit + cache_miss,
            completion_tokens: output_tokens,
            total_tokens: cache_hit + cache_miss + output_tokens,
            cache_hit_tokens: Some(cache_hit),
            cache_miss_tokens: Some(cache_miss),
        };

        let response = ChatResponse {
            content,
            reasoning,
            tool_calls,
            usage,
            finish_reason: normalize_stop_reason(&stop_reason),
            model: String::new(),
        };
        let _ = tx.send(Ok(StreamEvent::Done(Box::new(response))));
        Ok(())
    }
}

/// Maps Anthropic's `stop_reason` onto the same vocabulary the OpenAI
/// dialect produces -- `harness-agent` checks `finish_reason == "length"`
/// for truncation regardless of which dialect actually served the request.
fn normalize_stop_reason(reason: &str) -> String {
    match reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "end_turn" | "stop_sequence" => "stop",
        other => other,
    }
    .to_string()
}

// ---- request building ----

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<WireSystem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<WireThinking>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum WireSystem {
    Text(String),
    Blocks(Vec<WireSystemBlock>),
}

#[derive(Serialize)]
struct WireSystemBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    cache_control: WireCacheControl,
}

#[derive(Serialize)]
struct WireCacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct WireThinking {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: Vec<WireContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum WireContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Serialize)]
struct WireTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

/// Low-to-high token budgets for Anthropic's `thinking.budget_tokens`,
/// since Anthropic has no named effort tiers of its own -- these are a
/// judgment call, not a documented mapping. "none" and anything
/// unrecognized disables thinking rather than guessing.
fn thinking_budget_tokens(effort: &str) -> Option<u32> {
    match effort {
        "low" => Some(4096),
        "medium" => Some(8192),
        "high" => Some(16384),
        "xhigh" => Some(32768),
        "max" => Some(65536),
        _ => None,
    }
}

fn build_request(req: &ChatRequest) -> Result<WireRequest<'_>, String> {
    let thinking = req
        .reasoning_effort
        .as_deref()
        .and_then(thinking_budget_tokens)
        .map(|budget_tokens| WireThinking {
            kind: "enabled",
            budget_tokens,
        });

    // Anthropic requires max_tokens > thinking.budget_tokens; bump the
    // ceiling rather than let a well-formed request fail with a 400 that
    // has nothing to do with what the caller actually asked for.
    let base_max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let max_tokens = match &thinking {
        Some(t) => base_max_tokens.max(t.budget_tokens.saturating_add(1024)),
        None => base_max_tokens,
    };
    // Anthropic requires temperature be unset (or 1) when thinking is
    // enabled; sending both is a guaranteed 400.
    let temperature = if thinking.is_some() {
        None
    } else {
        req.temperature
    };

    let system_text: Vec<&str> = req
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.as_str())
        .filter(|c| !c.is_empty())
        .collect();
    let system = if system_text.is_empty() {
        None
    } else {
        let joined = system_text.join("\n\n");
        Some(if req.cache_prompt_prefix {
            WireSystem::Blocks(vec![WireSystemBlock {
                kind: "text",
                text: joined,
                cache_control: WireCacheControl { kind: "ephemeral" },
            }])
        } else {
            WireSystem::Text(joined)
        })
    };

    let messages = to_wire_messages(&req.messages)?;
    let tools = req
        .tools
        .iter()
        .map(|t| WireTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.parameters.clone(),
        })
        .collect();

    Ok(WireRequest {
        model: &req.model,
        max_tokens,
        messages,
        system,
        tools,
        stream: true,
        temperature,
        thinking,
    })
}

/// Non-system messages, Anthropic-shaped. Consecutive tool-result messages
/// (parallel tool calls) are merged into one `user` message carrying one
/// `tool_result` block per call -- Anthropic's documented convention,
/// unlike the OpenAI dialect's one-message-per-result.
fn to_wire_messages(msgs: &[Message]) -> Result<Vec<WireMessage>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    let non_system: Vec<&Message> = msgs.iter().filter(|m| m.role != Role::System).collect();

    while i < non_system.len() {
        let m = non_system[i];
        match m.role {
            Role::Tool => {
                let mut blocks = Vec::new();
                while i < non_system.len() && non_system[i].role == Role::Tool {
                    let tr = non_system[i];
                    let tool_use_id = tr
                        .tool_call_id
                        .clone()
                        .ok_or_else(|| "tool-result message missing tool_call_id".to_string())?;
                    blocks.push(WireContentBlock::ToolResult {
                        tool_use_id,
                        content: tr.content.clone(),
                    });
                    i += 1;
                }
                out.push(WireMessage {
                    role: "user",
                    content: blocks,
                });
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(WireContentBlock::Text {
                        text: m.content.clone(),
                    });
                }
                for tc in &m.tool_calls {
                    let input: serde_json::Value =
                        serde_json::from_str(tc.args.get()).unwrap_or(serde_json::json!({}));
                    blocks.push(WireContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input,
                    });
                }
                out.push(WireMessage {
                    role: "assistant",
                    content: blocks,
                });
                i += 1;
            }
            Role::User => {
                out.push(WireMessage {
                    role: "user",
                    content: vec![WireContentBlock::Text {
                        text: m.content.clone(),
                    }],
                });
                i += 1;
            }
            Role::System => unreachable!("filtered out above"),
        }
    }
    Ok(out)
}

// ---- streamed event shapes ----

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicEvent {
    MessageStart {
        message: MessageStartInner,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: ContentDelta,
    },
    ContentBlockStop {},
    MessageDelta {
        delta: MessageDeltaInner,
        #[serde(default)]
        usage: Option<MessageDeltaUsage>,
    },
    MessageStop {},
    Ping {},
    Error {
        error: AnthropicErrorBody,
    },
}

#[derive(Deserialize)]
struct MessageStartInner {
    usage: MessageStartUsage,
}

#[derive(Deserialize)]
struct MessageStartUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockStart {
    Text {},
    ToolUse { id: String, name: String },
    Thinking {},
}

// Every variant ends in "Delta" because that's Anthropic's own wire
// vocabulary (text_delta, input_json_delta, ...), not a naming smell.
#[allow(clippy::enum_variant_names)]
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    // The signature must be replayed verbatim if a `thinking` block is ever
    // sent back as history -- moot today, since reasoning is never replayed
    // upstream in either dialect (see `Message::reasoning`'s doc comment).
    SignatureDelta {},
}

#[derive(Deserialize)]
struct MessageDeltaInner {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct MessageDeltaUsage {
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct AnthropicErrorBody {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_types::ToolSchema;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves one canned SSE response over a real socket and hands back
    /// what the client actually sent, mirroring `client.rs`'s own test
    /// style so both dialects are exercised the same way.
    async fn serve(frames: Vec<&str>) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let owned: Vec<String> = frames.into_iter().map(str::to_string).collect();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut scratch = [0u8; 8192];
            let mut raw = Vec::new();
            let header_end = loop {
                let n = sock.read(&mut scratch).await.unwrap_or(0);
                raw.extend_from_slice(&scratch[..n]);
                if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let _ = tx.send(String::from_utf8_lossy(&raw[..header_end]).to_string());
            let mut out = String::from(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            for f in owned {
                out.push_str(&format!("data: {f}\n\n"));
            }
            let _ = sock.write_all(out.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        (format!("http://{addr}"), rx)
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: "claude-x".into(),
            messages: vec![Message::system("you are an agent"), Message::user("go")],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            cache_prompt_prefix: false,
        }
    }

    async fn collect(url: String) -> Vec<StreamEvent> {
        let client = AnthropicClient::new(url, "ak-abc");
        let mut rx = client.stream(&req());
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e.unwrap());
        }
        events
    }

    #[tokio::test]
    async fn the_api_key_header_is_sent_not_a_bearer_token() {
        let (url, rx) = serve(vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .await;
        let _events = collect(url).await;
        let sent = rx.await.unwrap().to_lowercase();
        assert!(sent.contains("x-api-key: ak-abc"), "{sent}");
        assert!(!sent.contains("authorization"), "{sent}");
        assert!(sent.contains("anthropic-version"), "{sent}");
    }

    #[tokio::test]
    async fn text_deltas_stream_and_the_done_event_carries_the_full_text() {
        let (url, _rx) = serve(vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .await;
        let events = collect(url).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello");
        let StreamEvent::Done(resp) = events.last().unwrap() else {
            panic!("expected Done")
        };
        assert_eq!(resp.content, "hello");
        assert_eq!(resp.finish_reason, "stop");
    }

    #[tokio::test]
    async fn a_tool_use_block_announces_once_and_assembles_its_input() {
        let (url, _rx) = serve(vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"read_file"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"th\":\".\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .await;
        let events = collect(url).await;
        let announced: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallStarted(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(announced, ["read_file"]);
        let StreamEvent::Done(resp) = events.last().unwrap() else {
            panic!("expected Done")
        };
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].args.get(), r#"{"path":"."}"#);
        assert_eq!(resp.finish_reason, "tool_calls");
    }

    #[tokio::test]
    async fn max_tokens_truncation_is_normalized_to_the_shared_length_value() {
        let (url, _rx) = serve(vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":100}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .await;
        let events = collect(url).await;
        let StreamEvent::Done(resp) = events.last().unwrap() else {
            panic!("expected Done")
        };
        assert_eq!(resp.finish_reason, "length");
    }

    #[tokio::test]
    async fn cache_read_tokens_count_as_hits_and_everything_else_as_misses() {
        let (url, _rx) = serve(vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":5,"cache_read_input_tokens":20}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .await;
        let events = collect(url).await;
        let StreamEvent::Done(resp) = events.last().unwrap() else {
            panic!("expected Done")
        };
        assert_eq!(resp.usage.cache_hit_tokens, Some(20));
        assert_eq!(resp.usage.cache_miss_tokens, Some(15));
        assert_eq!(resp.usage.prompt_tokens, 35);
        assert_eq!(resp.usage.completion_tokens, 7);
    }

    #[test]
    fn the_system_message_is_pulled_out_of_messages_into_its_own_field() {
        let r = req();
        let wire = build_request(&r).unwrap();
        assert!(matches!(wire.system, Some(WireSystem::Text(ref s)) if s == "you are an agent"));
        assert_eq!(wire.messages.len(), 1, "only the user turn remains");
    }

    #[test]
    fn cache_prompt_prefix_turns_the_system_field_into_a_cached_block() {
        let mut r = req();
        r.cache_prompt_prefix = true;
        let wire = build_request(&r).unwrap();
        assert!(matches!(wire.system, Some(WireSystem::Blocks(_))));
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let msgs = vec![
            Message::user("go"),
            Message::tool_result("c0", "read_file", "a"),
            Message::tool_result("c1", "search", "b"),
        ];
        let out = to_wire_messages(&msgs).unwrap();
        assert_eq!(
            out.len(),
            2,
            "the user turn, then one merged tool-result turn"
        );
        assert_eq!(out[1].content.len(), 2);
    }

    #[test]
    fn thinking_bumps_max_tokens_past_its_own_budget_and_drops_temperature() {
        let mut r = req();
        r.reasoning_effort = Some("high".to_string());
        r.temperature = Some(0.7);
        let wire = build_request(&r).unwrap();
        assert!(wire.max_tokens > 16384);
        assert!(wire.temperature.is_none());
    }

    #[test]
    fn an_unrecognized_effort_disables_thinking_rather_than_guessing() {
        let mut r = req();
        r.reasoning_effort = Some("ludicrous".to_string());
        let wire = build_request(&r).unwrap();
        assert!(wire.thinking.is_none());
    }

    #[test]
    fn tool_schemas_map_parameters_onto_input_schema() {
        let mut r = req();
        r.tools.push(ToolSchema {
            name: "search".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
        });
        let wire = build_request(&r).unwrap();
        assert_eq!(wire.tools.len(), 1);
        assert_eq!(
            wire.tools[0].input_schema,
            serde_json::json!({"type": "object"})
        );
    }
}
