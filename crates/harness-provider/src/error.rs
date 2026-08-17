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
    /// `1`: the count of attempts (`0` retries would be a contradiction in
    /// terms). `2`: the error the *last* attempt actually failed with --
    /// bare "gave up after 5 retries" said nothing about why, which reads
    /// identically whether the real cause was five straight connection
    /// refusals (offline) or five straight 503s (upstream is down) -- two
    /// situations that call for different things from the person reading
    /// it.
    #[error("gave up after {0} attempts -- last error: {1}")]
    RetriesExhausted(u32, Box<ProviderError>),
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
            // Terminal by definition -- there is no next attempt left to make.
            ProviderError::RetriesExhausted(..) => false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_names_both_the_attempt_count_and_the_actual_cause() {
        // Plain "gave up after 5 retries" -- the message this replaced --
        // read identically whether the real cause was being offline or the
        // upstream being down, which call for different next steps from
        // whoever is reading it.
        let last = ProviderError::Request("dns error: no such host".to_string());
        let exhausted = ProviderError::RetriesExhausted(6, Box::new(last));
        let text = exhausted.to_string();
        assert!(text.contains("6 attempts"), "{text}");
        assert!(text.contains("dns error"), "{text}");
    }

    #[test]
    fn exhausted_retries_is_never_itself_retryable() {
        // It exists precisely because there is no next attempt left to make.
        let inner = ProviderError::Http {
            status: 503,
            body: String::new(),
            retry_after_secs: None,
        };
        assert!(!ProviderError::RetriesExhausted(5, Box::new(inner)).is_retryable());
    }
}
