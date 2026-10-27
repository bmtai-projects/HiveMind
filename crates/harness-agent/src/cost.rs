//! Shared live-cost estimate -- the single source of truth both `Agent`
//! (for budget enforcement) and the host UI (for the per-turn readout)
//! compute from, so the two can never quietly disagree.

use harness_config::{Backend, lookup_model};
use harness_types::Usage;

pub fn estimate_cost_usd(usage: &Usage, model_id: &str, backend: Backend) -> Option<f64> {
    // A local model costs nothing, so quoting a figure derived from some
    // other provider's rate card would be inventing a charge.
    if !backend.is_billed() {
        return None;
    }
    // Hosted ignores it: the proxy's upstream charge excludes our markup.
    if backend == Backend::Byok
        && let Some(cost) = usage.reported_cost_usd
    {
        return Some(cost);
    }
    let pricing = lookup_model(model_id)?.wholesale_pricing;
    let miss = usage.cache_miss_tokens.unwrap_or(usage.prompt_tokens) as f64;
    let hit = usage.cache_hit_tokens.unwrap_or(0) as f64;
    let out = usage.completion_tokens as f64;
    let wholesale = (miss * pricing.input_per_m
        + hit * pricing.input_cache_read_per_m
        + out * pricing.output_per_m)
        / 1_000_000.0;
    Some(if backend.is_hosted() {
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
            ..Usage::default()
        }
    }

    #[test]
    fn a_byok_providers_reported_cost_wins_even_for_an_unlisted_model() {
        let reported = Usage {
            reported_cost_usd: Some(0.0042),
            ..usage(5_000, 100)
        };
        assert_eq!(
            estimate_cost_usd(&reported, "deepseek/deepseek-v4-flash", Backend::Byok),
            Some(0.0042)
        );
    }

    #[test]
    fn hosted_ignores_the_upstream_reported_cost() {
        let reported = Usage {
            reported_cost_usd: Some(9.0),
            ..usage(1_000_000, 0)
        };
        let hosted = estimate_cost_usd(&reported, "hivemind", Backend::Hosted).unwrap();
        assert!((hosted - 0.0826 * harness_config::HOSTED_MARKUP_MULTIPLIER).abs() < 1e-9);
    }

    #[test]
    fn the_quoted_markup_matches_the_one_the_server_actually_charges() {
        assert_eq!(
            harness_config::HOSTED_MARKUP_MULTIPLIER,
            1.15,
            "server default is 1.15; reconcile toward the server, it is what moves money"
        );
    }

    #[test]
    fn unknown_model_returns_none_not_zero() {
        assert_eq!(
            estimate_cost_usd(&usage(100, 10), "not-a-real-model", Backend::Hosted),
            None
        );
    }

    #[test]
    fn hosted_applies_the_markup_byok_does_not() {
        let hosted = estimate_cost_usd(&usage(1_000_000, 0), "hivemind", Backend::Hosted).unwrap();
        let byok = estimate_cost_usd(&usage(1_000_000, 0), "hivemind", Backend::Byok).unwrap();
        // hivemind's wholesale input_per_m is 0.0826 -- 1M miss tokens costs
        // exactly that wholesale, times the markup when hosted.
        assert!((byok - 0.0826).abs() < 1e-9);
        assert!((hosted - 0.0826 * harness_config::HOSTED_MARKUP_MULTIPLIER).abs() < 1e-9);
    }
}
