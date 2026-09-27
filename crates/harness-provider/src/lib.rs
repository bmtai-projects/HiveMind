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

mod client;
mod error;
mod retry;
mod wire;

pub use client::{ChatClient, EventStream, RetryHook};
pub use error::ProviderError;
pub use retry::{DEFAULT_MAX_RETRIES, backoff_delay};
