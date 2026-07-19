//! Exponential backoff with jitter, honoring a server's `Retry-After` when
//! present. Kept as free functions so `harness-agent` can reuse the same
//! policy for its own retry decisions without depending on the client.

use std::time::Duration;

pub const DEFAULT_MAX_RETRIES: u32 = 5;
const BASE_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 30_000;

/// Delay before the next attempt. `attempt` is 0-indexed (0 = first retry,
/// after the initial failed try). `retry_after_secs`, when the server sent
/// one, wins outright (capped so a misbehaving server can't stall forever).
pub fn backoff_delay(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    if let Some(secs) = retry_after_secs {
        return Duration::from_secs(secs.min(60));
    }
    let exp = attempt.min(6); // cap the exponent so this can't overflow or explode
    let base = BASE_DELAY_MS.saturating_mul(1u64 << exp);
    let jitter = rand::random::<u64>() % (base / 2 + 1);
    Duration::from_millis((base + jitter).min(MAX_DELAY_MS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_header_wins_and_is_capped() {
        assert_eq!(backoff_delay(0, Some(5)), Duration::from_secs(5));
        assert_eq!(backoff_delay(0, Some(999)), Duration::from_secs(60));
    }

    #[test]
    fn exponential_growth_is_bounded() {
        for attempt in 0..10 {
            let d = backoff_delay(attempt, None);
            assert!(d.as_millis() as u64 <= MAX_DELAY_MS);
        }
    }
}
