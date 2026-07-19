//! Streaming DeepSeek client — the only concrete backend this harness ships
//! today. Normalizes DeepSeek's OpenAI-compatible SSE dialect into
//! [`harness_types`], with connection reuse and transparent retry/backoff.
//!
//! Scoped deliberately: a second provider (OpenAI, Anthropic, xAI — any of
//! which is wire-compatible or a small adapter away) is a new module behind
//! the same [`DeepSeekClient::stream`] shape, not a rewrite.

mod client;
mod error;
mod retry;
mod wire;

pub use client::{DeepSeekClient, EventStream, RetryHook};
pub use error::ProviderError;
pub use retry::{DEFAULT_MAX_RETRIES, backoff_delay};
