use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("hivemind http {status}: {body}")]
    Http {
        status: u16,
        body: String,
        retry_after_secs: Option<u64>,
    },
    #[error("request failed: {0}")]
    Request(String),
    #[error("stream ended without a terminal response")]
    IncompleteStream,
    #[error("gave up after {0} retries")]
    RetriesExhausted(u32),
}

impl ProviderError {
    /// Whether a fresh attempt is worth trying: rate limits, server errors,
    /// and transport-level failures are transient; 4xx (auth, bad request)
    /// are not.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Http { status, .. } => *status == 429 || (500..600).contains(status),
            ProviderError::Request(_) => true,
            ProviderError::IncompleteStream => true,
            ProviderError::RetriesExhausted(_) => false,
        }
    }

    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            ProviderError::Http {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }
}
