//! Terminal implementation of [`harness_agent::Ui`]: ANSI-colored streaming
//! output plus a live cost readout computed from each response's usage and
//! the active tier's pricing — the whole point of the caching/tiering work
//! is to make that number small, so it's surfaced every turn, not hidden.

use std::io::{self, Write};
use std::sync::Mutex;
use std::time::Duration;

use harness_agent::Ui;
use harness_config::{ModelInfo, Pricing, Tier};
use harness_types::Usage;

pub struct TermUi {
    show_reasoning: bool,
    session_cost_usd: Mutex<f64>,
}

impl TermUi {
    pub fn new(show_reasoning: bool) -> Self {
        Self {
            show_reasoning,
            session_cost_usd: Mutex::new(0.0),
        }
    }

    /// Running total for a `/cost` command to read on demand.
    pub fn session_cost(&self) -> f64 {
        *self
            .session_cost_usd
            .lock()
            .expect("session cost mutex poisoned")
    }
}

impl Ui for TermUi {
    fn assistant_delta(&self, text: &str) {
        print!("{text}");
        flush_stdout();
    }

    fn reasoning_delta(&self, text: &str) {
        if self.show_reasoning {
            print!("\x1b[90m{text}\x1b[0m");
            flush_stdout();
        }
    }

    fn assistant_done(&self) {
        println!();
    }

    fn tool_start(&self, name: &str, args: &str) {
        println!("\x1b[36m⚙ {name}\x1b[0m {}", one_line(args, 140));
    }

    fn tool_end(&self, name: &str, result: &str, is_error: bool) {
        if is_error {
            println!("\x1b[31m  ✗ {name}\x1b[0m {}", one_line(result, 160));
        } else {
            println!("\x1b[32m  ✓ {name}\x1b[0m {}", one_line(result, 160));
        }
    }

    fn usage(&self, usage: &Usage, tier: Tier, model: &ModelInfo) {
        if usage.total_tokens == 0 {
            return;
        }
        let turn_cost = estimate_cost_usd(usage, &model.pricing);
        let session_total = {
            let mut total = self
                .session_cost_usd
                .lock()
                .expect("session cost mutex poisoned");
            *total += turn_cost;
            *total
        };
        let cache_note = usage
            .cache_hit_rate()
            .map(|r| format!(", cache {:.0}%", r * 100.0))
            .unwrap_or_default();
        // 6 decimals: a single Flash turn is routinely sub-$0.0001 — at 4
        // decimals the running total looked like a stuck "$0.0000" even
        // while correctly accumulating (caught by end-to-end testing, not
        // a logic bug — just not enough resolution to show it).
        println!(
            "\x1b[90m  ↳ [{tier}] {} in / {} out{cache_note} · ${turn_cost:.6} turn / ${session_total:.6} session\x1b[0m",
            usage.prompt_tokens, usage.completion_tokens,
        );
    }

    fn retrying(&self, attempt: u32, max: u32, delay: Duration, err: &str) {
        eprintln!(
            "\x1b[33m  ⚠ retry {attempt}/{max} in {:.1}s: {err}\x1b[0m",
            delay.as_secs_f64()
        );
    }

    fn tier_escalated(&self, from: Tier, to: Tier, reason: &str) {
        println!("\x1b[35m  ⤴ escalating {from} → {to}: {reason}\x1b[0m");
    }

    fn compacted(&self, messages_before: usize, messages_after: usize, tokens_before: u64) {
        println!(
            "\x1b[90m  ⤵ compacted context: {messages_before} → {messages_after} messages ({tokens_before} tokens before)\x1b[0m"
        );
    }
}

/// DeepSeek bills cache-miss and cache-hit prompt tokens at different rates;
/// when a provider doesn't report the split, treat the whole prompt as a
/// cache miss (the conservative, never-underestimate default).
fn estimate_cost_usd(usage: &Usage, pricing: &Pricing) -> f64 {
    let miss = usage.cache_miss_tokens.unwrap_or(usage.prompt_tokens) as f64;
    let hit = usage.cache_hit_tokens.unwrap_or(0) as f64;
    let out = usage.completion_tokens as f64;
    (miss * pricing.input_cache_miss_per_m
        + hit * pricing.input_cache_hit_per_m
        + out * pricing.output_per_m)
        / 1_000_000.0
}

fn one_line(s: &str, max_chars: usize) -> String {
    let collapsed = s.replace('\n', " ");
    let trimmed = collapsed.trim();
    if trimmed.chars().count() > max_chars {
        let head: String = trimmed.chars().take(max_chars).collect();
        format!("{head}…")
    } else {
        trimmed.to_string()
    }
}

pub fn flush_stdout() {
    let _ = io::stdout().flush();
}

/// Blocking stdin prompt for shell-command approval. Always invoked through
/// `tokio::task::spawn_blocking` by [`harness_tools::Bash`], so blocking
/// here never stalls the async runtime.
pub fn terminal_approve(cmd: &str) -> bool {
    print!("\n\x1b[33m▶ run shell command?\x1b[0m {cmd}\n  [y/N] ");
    flush_stdout();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}
