//! Shared live-cost estimate -- the single source of truth both `Agent`
//! (for budget enforcement) and the host UI (for the per-turn readout)
//! compute from, so the two can never quietly disagree.

use harness_config::lookup_model;
use harness_types::Usage;

/// A model's provider bills cache-miss and cache-hit prompt tokens at
/// different rates; when a response doesn't report the split, treat the
/// whole prompt as a cache miss (the conservative, never-underestimate
/// default). `hosted` applies HiveMind's margin on top of the wholesale
/// price, since that's what a hosted user is actually billed; a BYOK key
/// pays the upstream provider's wholesale price directly. `None` for a
/// `model_id` not in `KNOWN_MODELS` (a BYOK user's own custom string) --
/// there's no pricing data to estimate from, not a zero cost.
pub fn estimate_cost_usd(usage: &Usage, model_id: &str, hosted: bool) -> Option<f64> {
    let pricing = lookup_model(model_id)?.wholesale_pricing;
    let miss = usage.cache_miss_tokens.unwrap_or(usage.prompt_tokens) as f64;
    let hit = usage.cache_hit_tokens.unwrap_or(0) as f64;
    let out = usage.completion_tokens as f64;
    let wholesale = (miss * pricing.input_per_m
        + hit * pricing.input_cache_read_per_m
        + out * pricing.output_per_m)
        / 1_000_000.0;
    Some(if hosted {
        wholesale * harness_config::HOSTED_MARKUP_MULTIPLIER
    } else {
        wholesale
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u64, completion: u64) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cache_hit_tokens: None,
            cache_miss_tokens: None,
        }
    }

    #[test]
    fn unknown_model_returns_none_not_zero() {
        assert_eq!(
            estimate_cost_usd(&usage(100, 10), "not-a-real-model", true),
            None
        );
    }

    #[test]
    fn hosted_applies_the_markup_byok_does_not() {
        let hosted = estimate_cost_usd(&usage(1_000_000, 0), "hivemind", true).unwrap();
        let byok = estimate_cost_usd(&usage(1_000_000, 0), "hivemind", false).unwrap();
        // hivemind's wholesale input_per_m is 0.0938 -- 1M miss tokens costs
        // exactly that wholesale, times the markup when hosted.
        assert!((byok - 0.0938).abs() < 1e-9);
        assert!((hosted - 0.0938 * harness_config::HOSTED_MARKUP_MULTIPLIER).abs() < 1e-9);
    }
}
