//! Streaming client for the OpenAI-compatible Chat Completions dialect —
//! normalized into [`harness_types`], with connection reuse and transparent
//! retry/backoff.
//!
//! One client covers every model in the catalog: hosted models are reached
//! through HiveMind's own proxy, and BYOK keys talk to their vendor
//! directly, but both speak the same dialect over `{base_url}/chat/
//! completions` with a bearer token. Nothing here is specific to any one
//! provider.
//!
//! A provider speaking a genuinely different wire format would be a new
//! module behind the same [`ChatClient::stream`] shape, not a rewrite.

mod anthropic;
mod client;
mod error;
mod models;
mod retry;
mod wire;

use harness_types::ChatRequest;

pub use anthropic::AnthropicClient;
pub use client::{ChatClient, EventStream, RetryHook};
pub use error::ProviderError;
pub use models::{ModelsFuture, RemoteModel};
pub use retry::{DEFAULT_MAX_RETRIES, backoff_delay};

/// One dialect that can stream a sampling request, behind a single object
/// so `harness-agent` can hold either without knowing which it has. Not
/// `#[async_trait]`: `stream` itself is synchronous -- it spawns its own
/// background task and hands back a channel -- so an ordinary object-safe
/// trait is enough.
pub trait Provider: Send + Sync {
    fn stream(&self, req: &ChatRequest) -> EventStream;
    /// Pre-open the TLS connection. A no-op default so this stays optional
    /// per dialect rather than forcing every implementor to care.
    fn warm(&self) {}
    /// The models this provider offers, as it describes them. Providers that
    /// have no such list answer with an error, which callers treat as "unknown".
    fn list_models(&self) -> ModelsFuture<'_> {
        Box::pin(async {
            Err(ProviderError::Request(
                "this provider does not list its models".into(),
            ))
        })
    }
    /// The models people most use for programming, best first, when the
    /// provider keeps such a list. Empty when it does not.
    fn list_popular(&self) -> ModelsFuture<'_> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl Provider for ChatClient {
    fn stream(&self, req: &ChatRequest) -> EventStream {
        ChatClient::stream(self, req)
    }
    fn warm(&self) {
        ChatClient::warm(self)
    }
    fn list_models(&self) -> ModelsFuture<'_> {
        Box::pin(ChatClient::list_models(self))
    }
    fn list_popular(&self) -> ModelsFuture<'_> {
        Box::pin(ChatClient::list_popular(self))
    }
}

impl Provider for AnthropicClient {
    fn stream(&self, req: &ChatRequest) -> EventStream {
        AnthropicClient::stream(self, req)
    }
    fn warm(&self) {
        AnthropicClient::warm(self)
    }
    fn list_models(&self) -> ModelsFuture<'_> {
        Box::pin(AnthropicClient::list_models(self))
    }
}
