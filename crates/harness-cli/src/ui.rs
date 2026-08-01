//! Terminal implementation of [`harness_agent::Ui`]: ANSI-colored streaming
//! output plus a live cost readout computed from each response's usage and
//! the active model's pricing — the whole point of the caching/model-tiering
//! work is to make that number small, so it's surfaced every turn, not hidden.

use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use harness_agent::{Ui, estimate_cost_usd};
use harness_types::Usage;

pub struct TermUi {
    show_reasoning: bool,
    /// Display-only -- `Agent` is the actual source of truth for
    /// cumulative spend and the budget it's checked against (it has to
    /// track both anyway, to enforce it). Mutable because `/budget` can
    /// change it mid-session; kept in sync by the REPL dispatch site
    /// alongside the matching `Agent::set_budget_usd` call. Shown on every
    /// turn's usage line when set, so headroom is visible continuously,
    /// not just at the moment the cap is hit.
    budget_usd: Mutex<Option<f64>>,
    /// Whether the "thinking..." indicator has already fired for the turn
    /// currently in flight -- reset in `usage()`, which fires exactly once
    /// per turn right after the full response (reasoning, content, and any
    /// tool calls together) finishes draining. This is deliberately a
    /// lightweight always-on signal, separate from `show_reasoning`: a user
    /// should always be able to tell the model is thinking (not stalled),
    /// even if they never want the full raw chain-of-thought dumped to
    /// the terminal.
    thinking_shown: AtomicBool,
    /// Tool names streamed so far this turn, rendered as one rewritable
    /// preview line. Erased by whatever prints next -- it's a latency hint,
    /// not transcript. Empty means no preview is on screen.
    pending_calls: Mutex<Vec<String>>,
    /// Cursor rewriting only works on a real terminal; piped output (tests,
    /// `| tee`) gets the normal lines and no preview.
    interactive: bool,
}

impl TermUi {
    pub fn new(show_reasoning: bool, budget_usd: Option<f64>) -> Self {
        Self {
            show_reasoning,
            budget_usd: Mutex::new(budget_usd),
            thinking_shown: AtomicBool::new(false),
            pending_calls: Mutex::new(Vec::new()),
            interactive: io::stdout().is_terminal(),
        }
    }

    /// Wipe the preview line if one is showing, so the caller can print
    /// normally. Idempotent.
    fn clear_preview(&self) {
        let mut pending = self.pending_calls.lock().expect("preview mutex poisoned");
        if pending.is_empty() {
            return;
        }
        pending.clear();
        print!("\r\x1b[2K");
        flush_stdout();
    }

    /// Keep the displayed budget in sync with `Agent::set_budget_usd` --
    /// call both together (see the REPL's `/budget` dispatch).
    pub fn set_budget_display(&self, budget_usd: Option<f64>) {
        *self.budget_usd.lock().expect("budget mutex poisoned") = budget_usd;
    }
}

impl Ui for TermUi {
    fn turn_started(&self) {
        // Unconditional -- unlike `reasoning_delta` below, this doesn't
        // depend on the model actually streaming chain-of-thought, so it's
        // the one signal guaranteed to show up the instant a request goes
        // out. `reasoning_delta`'s own swap becomes a no-op right after
        // this (already-true), so there's no double print if reasoning
        // does show up.
        self.thinking_shown.store(true, Ordering::Relaxed);
        println!("\x1b[2;3m⟡ thinking...\x1b[0m");
    }

    fn assistant_delta(&self, text: &str) {
        self.clear_preview();
        print!("{text}");
        flush_stdout();
    }

    fn tool_call_pending(&self, name: &str) {
        if !self.interactive {
            return;
        }
        let mut pending = self.pending_calls.lock().expect("preview mutex poisoned");
        pending.push(name.to_string());
        print!("\r\x1b[2K\x1b[36m⚙ {}\x1b[0m \x1b[90m…\x1b[0m", pending.join(", "));
        flush_stdout();
    }

    fn reasoning_delta(&self, text: &str) {
        // Fires once per turn, on the very first reasoning fragment --
        // works for every reasoning-capable model, including the ones that
        // reason unconditionally (see harness_config::ModelCatalogEntry's
        // reasoning_efforts doc comment) and never had a reasoning_effort
        // request sent for them at all.
        if !self.thinking_shown.swap(true, Ordering::Relaxed) {
            println!("\x1b[2;3m⟡ thinking...\x1b[0m");
        }
        if self.show_reasoning {
            self.clear_preview();
            print!("\x1b[90m{text}\x1b[0m");
            flush_stdout();
        }
    }

    fn assistant_done(&self) {
        self.clear_preview();
        println!();
    }

    fn tool_start(&self, name: &str, args: &str) {
        self.clear_preview();
        println!("\x1b[36m⚙ {name}\x1b[0m {}", one_line(args, 140));
    }

    fn tool_end(&self, name: &str, result: &str, is_error: bool) {
        if is_error {
            println!("\x1b[31m  ✗ {name}\x1b[0m {}", one_line(result, 160));
            return;
        }
        // todo_write's result is a multi-line checklist -- one_line() would
        // collapse it to an unreadable single line, defeating the entire
        // point of a visible plan. Every other tool's result is fine
        // flattened; this is the one deliberate exception.
        if name == "todo_write" {
            println!("\x1b[32m  ✓ {name}\x1b[0m");
            for line in result.lines() {
                println!("\x1b[90m      {line}\x1b[0m");
            }
            return;
        }
        println!("\x1b[32m  ✓ {name}\x1b[0m {}", one_line(result, 160));
    }

    fn usage(&self, usage: &Usage, model_id: &str, hosted: bool, session_cost_usd: f64) {
        // Fires unconditionally, ahead of the early return below -- this is
        // the one guaranteed once-per-turn boundary, so it's the correct
        // place to re-arm the thinking indicator for the next turn.
        self.clear_preview();
        self.thinking_shown.store(false, Ordering::Relaxed);
        if usage.total_tokens == 0 {
            return;
        }
        let cache_note = usage
            .cache_hit_rate()
            .map(|r| format!(", cache {:.0}%", r * 100.0))
            .unwrap_or_default();
        let budget_note = self
            .budget_usd
            .lock()
            .expect("budget mutex poisoned")
            .map(|b| format!(" (of ${b:.2} budget)"))
            .unwrap_or_default();

        // Unrecognized model id (a BYOK user's own custom string, not in
        // KNOWN_MODELS) -- show token counts with no cost estimate rather
        // than a wrong or fabricated one.
        let Some(turn_cost) = estimate_cost_usd(usage, model_id, hosted) else {
            println!(
                "\x1b[90m  ↳ [{model_id}] {} in / {} out{cache_note}\x1b[0m",
                usage.prompt_tokens, usage.completion_tokens,
            );
            return;
        };
        // 6 decimals: a single "hivemind" turn is routinely sub-$0.0001 —
        // at 4 decimals the running total looked like a stuck "$0.0000"
        // even while correctly accumulating (caught by end-to-end testing,
        // not a logic bug — just not enough resolution to show it).
        println!(
            "\x1b[90m  ↳ [{model_id}] {} in / {} out{cache_note} · ${turn_cost:.6} turn / ${session_cost_usd:.6} session{budget_note}\x1b[0m",
            usage.prompt_tokens, usage.completion_tokens,
        );
    }

    fn retrying(&self, attempt: u32, max: u32, delay: Duration, err: &str) {
        eprintln!(
            "\x1b[33m  ⚠ retry {attempt}/{max} in {:.1}s: {err}\x1b[0m",
            delay.as_secs_f64()
        );
    }

    fn model_escalated(&self, from: &str, to: &str, reason: &str) {
        println!("\x1b[35m  ⤴ escalating {from} → {to}: {reason}\x1b[0m");
    }

    fn interjected(&self, count: usize) {
        let noun = if count == 1 { "message" } else { "messages" };
        println!("\x1b[36m  ↩ delivered your {noun} to the model\x1b[0m");
    }

    fn tool_progress(&self, tool: &str, message: &str) {
        eprintln!("\x1b[90m  {tool}: {message}\x1b[0m");
    }

    fn context_trimmed(&self, results_elided: usize, tokens_saved: u64) {
        self.clear_preview();
        let noun = if results_elided == 1 { "result" } else { "results" };
        println!(
            "\x1b[90m  ⤵ freed ~{tokens_saved} tokens ({results_elided} old tool {noun} dropped)\x1b[0m"
        );
    }

    fn compacted(
        &self,
        messages_before: usize,
        messages_after: usize,
        tokens_before: u64,
        summary_cost_usd: Option<f64>,
    ) {
        let cost_suffix = match summary_cost_usd {
            Some(cost) => format!(", summary cost ${cost:.6}"),
            None => String::new(),
        };
        println!(
            "\x1b[90m  ⤵ compacted context: {messages_before} → {messages_after} messages ({tokens_before} tokens before{cost_suffix})\x1b[0m"
        );
    }

    fn stopped_for_budget(&self, spent_usd: f64, budget_usd: f64) {
        println!(
            "\x1b[33m⛔ stopped: session cost ${spent_usd:.6} has reached the ${budget_usd:.2} budget\x1b[0m"
        );
        println!("\x1b[90m  raise it with --budget, or /budget <amount>, or /budget off\x1b[0m");
    }

    fn stopped_for_context_limit(&self, estimated_tokens: u64, context_window: u64) {
        println!(
            "\x1b[33m⛔ stopped: this request is ~{estimated_tokens} tokens, too large for the active model's {context_window}-token context window\x1b[0m"
        );
        println!(
            "\x1b[90m  trim the input, /clear and start fresh, or /model to a bigger-context one\x1b[0m"
        );
    }
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
