use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::Client;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use harness_types::{ChatRequest, ChatResponse, StreamEvent, Usage};

use crate::error::ProviderError;
use crate::retry::{DEFAULT_MAX_RETRIES, backoff_delay};
use crate::wire::{self, WireRequest, WireStreamChunk, WireStreamOptions};

pub type EventStream = UnboundedReceiver<Result<StreamEvent, ProviderError>>;

/// Called before each retry sleep: `(attempt, of_max, delay, error)`. Lets
/// the CLI surface "rate limited, retrying in 2s..." without this crate
/// depending on a logging framework or knowing about terminals.
pub type RetryHook = Arc<dyn Fn(u32, u32, Duration, &ProviderError) + Send + Sync>;

/// Streaming DeepSeek client. Holds one [`reqwest::Client`] — cloning
/// `DeepSeekClient` clones an `Arc`-backed handle to the same connection
/// pool, so every request (including retries and concurrent tool-triggered
/// escalation calls) reuses keep-alive HTTP/2 connections rather than
/// paying a fresh TLS handshake each time.
#[derive(Clone)]
pub struct DeepSeekClient {
    http: Client,
    base_url: Arc<str>,
    api_key: Arc<str>,
    max_retries: u32,
    on_retry: Option<RetryHook>,
}

impl DeepSeekClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(300))
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

    /// Start one streaming sampling request. Borrows `req` only long enough
    /// to serialize it (synchronously, before any `.await`) — the caller
    /// gets `req`'s ownership straight back, so driving a multi-turn
    /// conversation never needs to clone the (potentially large) message
    /// history just to send it. Connection, retry loop, and SSE decode all
    /// then run in a spawned task against the pre-serialized bytes, so
    /// transport failures (429/5xx/network) are retried transparently and
    /// never reach the caller as an error unless retries are exhausted.
    pub fn stream(&self, req: &ChatRequest) -> EventStream {
        let (tx, rx) = mpsc::unbounded_channel();

        let wire_req = WireRequest {
            model: &req.model,
            messages: wire::to_wire_messages(&req.messages),
            tools: wire::to_wire_tools(&req.tools),
            stream: true,
            stream_options: WireStreamOptions {
                include_usage: true,
            },
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            reasoning_effort: req.reasoning_effort.as_deref(),
        };
        let body: Bytes = match serde_json::to_vec(&wire_req) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                let _ = tx.send(Err(ProviderError::Request(e.to_string())));
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
                    let _ = tx.send(Err(e));
                    return;
                }
            }
        }
    }

    /// `body` is a cheap refcounted clone per attempt (not a copy) — only
    /// the first serialization in [`Self::stream`] pays for encoding.
    async fn try_once(
        &self,
        body: Bytes,
        tx: &UnboundedSender<Result<StreamEvent, ProviderError>>,
    ) -> Result<(), ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("Authorization", format!("Bearer {}", self.api_key))
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
        let mut finish_reason = String::new();
        let mut usage = Usage::default();

        struct Acc {
            id: String,
            name: String,
            args: String,
        }
        let mut by_index: BTreeMap<usize, Acc> = BTreeMap::new();
        let mut saw_any_frame = false;

        'outer: while let Some(chunk) = byte_stream.next().await {
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
                if data == "[DONE]" {
                    break 'outer;
                }

                let parsed: WireStreamChunk = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue, // tolerate keep-alives / malformed frames
                };
                saw_any_frame = true;

                if let Some(u) = parsed.usage {
                    usage = Usage {
                        prompt_tokens: u.prompt_tokens,
                        completion_tokens: u.completion_tokens,
                        total_tokens: u.total_tokens,
                        cache_hit_tokens: u.prompt_cache_hit_tokens,
                        cache_miss_tokens: u.prompt_cache_miss_tokens,
                    };
                }

                for choice in parsed.choices {
                    if let Some(c) = choice.delta.content
                        && !c.is_empty()
                    {
                        content.push_str(&c);
                        let _ = tx.send(Ok(StreamEvent::TextDelta(c)));
                    }
                    if let Some(r) = choice.delta.reasoning_content
                        && !r.is_empty()
                    {
                        reasoning.push_str(&r);
                        let _ = tx.send(Ok(StreamEvent::ReasoningDelta(r)));
                    }
                    for tc in choice.delta.tool_calls {
                        let entry = by_index.entry(tc.index).or_insert_with(|| Acc {
                            id: String::new(),
                            name: String::new(),
                            args: String::new(),
                        });
                        if let Some(id) = tc.id {
                            entry.id = id;
                        }
                        if let Some(f) = tc.function {
                            if let Some(name) = f.name {
                                entry.name = name;
                            }
                            if let Some(args) = f.arguments {
                                entry.args.push_str(&args);
                            }
                        }
                    }
                    if let Some(fr) = choice.finish_reason {
                        finish_reason = fr;
                    }
                }
            }
        }

        if !saw_any_frame {
            return Err(ProviderError::IncompleteStream);
        }

        let tool_calls = by_index
            .into_values()
            .map(|acc| wire::tool_call(acc.id, acc.name, &acc.args))
            .collect();

        let response = ChatResponse {
            content,
            reasoning,
            tool_calls,
            usage,
            finish_reason,
            model: String::new(),
        };
        let _ = tx.send(Ok(StreamEvent::Done(Box::new(response))));
        Ok(())
    }
}
