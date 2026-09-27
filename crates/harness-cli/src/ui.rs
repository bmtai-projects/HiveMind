//! Terminal implementation of [`harness_agent::Ui`]: ANSI-colored streaming
//! output plus a live cost readout computed from each response's usage and
//! the active model's pricing — the whole point of the caching/model-tiering
//! work is to make that number small, so it's surfaced every turn, not hidden.

use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use harness_agent::{Ui, estimate_cost_usd};
use harness_config::Backend;
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
    /// Terminal state, and its write lock. Lock order: `screen` → `Stdout`.
    screen: Mutex<Screen>,
    /// Cursor rewriting only works on a real terminal; piped output (tests,
    /// `| tee`) gets the normal lines and no preview.
    interactive: bool,
}

/// Where the cursor sits, and so what the next write must do first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cursor {
    LineStart,
    /// Unclosed prose. Real content: close it, never erase it.
    AfterProse,
    /// The rewritable preview. Disposable: erase in place.
    AfterPreview,
}

struct Screen {
    /// Tool names in the current preview line.
    pending: Vec<String>,
    cursor: Cursor,
}

impl TermUi {
    pub fn new(show_reasoning: bool, budget_usd: Option<f64>) -> Self {
        Self {
            show_reasoning,
            budget_usd: Mutex::new(budget_usd),
            thinking_shown: AtomicBool::new(false),
            screen: Mutex::new(Screen {
                pending: Vec::new(),
                cursor: Cursor::LineStart,
            }),
            interactive: io::stdout().is_terminal(),
        }
    }

    /// Return to column 0 without destroying content. Idempotent.
    fn end_line(&self) {
        let mut s = self.screen.lock().expect("screen mutex poisoned");
        if s.cursor == Cursor::LineStart {
            return;
        }
        let mut out = io::stdout().lock();
        match s.cursor {
            Cursor::AfterPreview => {
                let _ = out.write_all(b"\r\x1b[2K");
                s.pending.clear();
            }
            Cursor::AfterProse => {
                let _ = out.write_all(b"\n");
            }
            Cursor::LineStart => unreachable!("early-returned above"),
        }
        s.cursor = Cursor::LineStart;
        let _ = out.flush();
    }

    /// Write `body` as one complete line, atomically against other threads.
    fn emit_line(&self, body: &str) {
        self.emit(body, true);
    }

    /// As `emit_line`, but leaves the line open for more streamed text.
    fn emit_partial(&self, body: &str) {
        self.emit(body, false);
    }

    fn emit(&self, body: &str, newline: bool) {
        let mut s = self.screen.lock().expect("screen mutex poisoned");
        // One lock across preamble and body, so writes cannot interleave.
        let mut out = io::stdout().lock();
        let _ = out.write_all(preamble(s.cursor, newline).as_bytes());
        if s.cursor == Cursor::AfterPreview {
            s.pending.clear();
        }
        let _ = out.write_all(body.as_bytes());
        if newline {
            let _ = out.write_all(b"\n");
            s.cursor = Cursor::LineStart;
        } else if !body.is_empty() {
            s.cursor = Cursor::AfterProse;
        }
        let _ = out.flush();
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
        self.emit_line("\x1b[2;3m⟡ thinking...\x1b[0m");
    }

    fn assistant_delta(&self, text: &str) {
        self.emit_partial(text);
    }

    fn tool_call_pending(&self, name: &str) {
        if !self.interactive {
            return;
        }
        let mut s = self.screen.lock().expect("screen mutex poisoned");
        s.pending.push(name.to_string());
        let preview = format!(
            "\x1b[36m⚙ {}\x1b[0m \x1b[90m…\x1b[0m",
            one_line(&s.pending.join(", "), width().saturating_sub(6))
        );
        // Takes a full line's preamble, so it never erases unclosed prose.
        let mut out = io::stdout().lock();
        let _ = out.write_all(preamble(s.cursor, true).as_bytes());
        let _ = out.write_all(preview.as_bytes());
        s.cursor = Cursor::AfterPreview;
        let _ = out.flush();
    }

    fn reasoning_delta(&self, text: &str) {
        // Fires once per turn, on the very first reasoning fragment --
        // works for every reasoning-capable model, including the ones that
        // reason unconditionally (see harness_config::ModelCatalogEntry's
        // reasoning_efforts doc comment) and never had a reasoning_effort
        // request sent for them at all.
        if !self.thinking_shown.swap(true, Ordering::Relaxed) {
            self.emit_line("\x1b[2;3m⟡ thinking...\x1b[0m");
        }
        if self.show_reasoning {
            self.emit_partial(&format!("\x1b[90m{text}\x1b[0m"));
        }
    }

    fn assistant_done(&self) {
        self.end_line();
    }

    fn tool_start(&self, name: &str, args: &str) {
        // 4 = "⚙ " prefix plus the space after the padded name.
        let room = width().saturating_sub(TOOL_NAME_WIDTH + 4);
        self.emit_line(&format!(
            "\x1b[36m⚙ {:<TOOL_NAME_WIDTH$}\x1b[0m {}",
            name,
            one_line(&describe_args(name, args), room)
        ));
    }

    fn tool_end(
        &self,
        name: &str,
        result: &str,
        is_error: bool,
        cost_usd: f64,
        session_cost_usd: f64,
    ) {
        let cost = if cost_usd > 0.0 {
            format!(" · ${cost_usd:.6} / ${session_cost_usd:.6} session")
        } else {
            String::new()
        };
        // Errors keep their raw text: it is what the model acts on.
        if is_error {
            let room = width().saturating_sub(TOOL_NAME_WIDTH + 6 + cost.chars().count());
            self.emit_line(&format!(
                "\x1b[31m  ✗ {:<TOOL_NAME_WIDTH$}\x1b[0m {}{cost}",
                name,
                one_line(result, room)
            ));
            return;
        }
        // todo_write's result is a multi-line checklist -- one_line() would
        // collapse it to an unreadable single line, defeating the entire
        // point of a visible plan. Every other tool's result is fine
        // flattened; this is the one deliberate exception.
        if name == "todo_write" {
            let mut block = format!("\x1b[32m  ✓ {name}\x1b[0m");
            for line in result.lines() {
                block.push_str(&format!("\n\x1b[90m      {line}\x1b[0m"));
            }
            self.emit_line(&block);
            return;
        }
        let room = width().saturating_sub(TOOL_NAME_WIDTH + 6 + cost.chars().count());
        self.emit_line(&format!(
            "\x1b[32m  ✓ {:<TOOL_NAME_WIDTH$}\x1b[0m \x1b[90m{}\x1b[0m{cost}",
            name,
            one_line(&describe_result(name, result), room)
        ));
    }

    fn usage(&self, usage: &Usage, model_id: &str, backend: Backend, session_cost_usd: f64) {
        // Fires unconditionally, ahead of the early return below -- this is
        // the one guaranteed once-per-turn boundary, so it's the correct
        // place to re-arm the thinking indicator for the next turn.
        self.thinking_shown.store(false, Ordering::Relaxed);
        if usage.total_tokens == 0 {
            self.end_line();
            return;
        }
        let cache_note = usage
            .cache_hit_rate()
            .map(|r| format!(", cache {:.0}%", r * 100.0))
            .unwrap_or_default();
        // Resolved first, so this guard is dropped before `screen` is taken.
        let budget_note = self
            .budget_usd
            .lock()
            .expect("budget mutex poisoned")
            .map(|b| format!(" (of ${b:.2} budget)"))
            .unwrap_or_default();

        // Unrecognized model id (a BYOK user's own custom string, not in
        // KNOWN_MODELS) -- show token counts with no cost estimate rather
        // than a wrong or fabricated one.
        let Some(turn_cost) = estimate_cost_usd(usage, model_id, backend) else {
            self.emit_line(&format!(
                "\x1b[90m  ↳ [{model_id}] {} in / {} out{cache_note}\x1b[0m",
                usage.prompt_tokens, usage.completion_tokens,
            ));
            return;
        };
        // 6 decimals: a single "hivemind" turn is routinely sub-$0.0001 —
        // at 4 decimals the running total looked like a stuck "$0.0000"
        // even while correctly accumulating (caught by end-to-end testing,
        // not a logic bug — just not enough resolution to show it).
        self.emit_line(&format!(
            "\x1b[90m  ↳ [{model_id}] {} in / {} out{cache_note} · ${turn_cost:.6} turn / ${session_cost_usd:.6} session{budget_note}\x1b[0m",
            usage.prompt_tokens, usage.completion_tokens,
        ));
    }

    fn retrying(&self, attempt: u32, max: u32, delay: Duration, err: &str) {
        eprintln!(
            "\x1b[33m  ⚠ retry {attempt}/{max} in {:.1}s: {err}\x1b[0m",
            delay.as_secs_f64()
        );
    }

    fn stalled(&self, after_turns: u32) {
        self.emit_line(&format!(
            "\x1b[33m  ↯ no progress in {after_turns} turns — asked the model to reconsider\x1b[0m"
        ));
    }

    fn validation_required(&self, changed_files: usize) {
        let s = if changed_files == 1 { "" } else { "s" };
        self.emit_line(&format!(
            "\x1b[33m  ↯ finished with {changed_files} changed file{s} and no check run — asked the model to verify\x1b[0m"
        ));
    }

    fn turns_extended(&self, turns_used: u32, new_limit: u32) {
        self.emit_line(&format!(
            "\x1b[33m  ↻ still making progress at {turns_used} turns — continuing to {new_limit}\x1b[0m"
        ));
    }

    fn escalation_declined(&self, to: &str, spent: f64, budget: f64) {
        self.emit_line(&format!(
            "\x1b[33m  ↯ staying on this model — switching to {to} would cost far more per token \
             and ${spent:.4} of the ${budget:.2} budget is already spent\x1b[0m"
        ));
    }

    fn output_limit_truncated(&self) {
        self.emit_line(
            "\x1b[33m  ↯ hit the output limit mid-tool-call — dropped it and asked for smaller steps\x1b[0m"
        );
    }

    fn model_escalated(&self, from: &str, to: &str, reason: &str) {
        self.emit_line(&format!(
            "\x1b[35m  ⤴ escalating {from} → {to}: {reason}\x1b[0m"
        ));
    }

    fn interjected(&self, count: usize) {
        let noun = if count == 1 { "message" } else { "messages" };
        self.emit_line(&format!(
            "\x1b[36m  ↩ delivered your {noun} to the model\x1b[0m"
        ));
    }

    fn tool_progress(&self, tool: &str, message: &str) {
        eprintln!("\x1b[90m  {tool}: {message}\x1b[0m");
    }

    fn context_trimmed(&self, results_elided: usize, tokens_saved: u64) {
        let noun = if results_elided == 1 {
            "result"
        } else {
            "results"
        };
        self.emit_line(&format!(
            "\x1b[90m  ⤵ freed ~{tokens_saved} tokens ({results_elided} old tool {noun} dropped)\x1b[0m"
        ));
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
        self.emit_line(&format!(
            "\x1b[90m  ⤵ compacted context: {messages_before} → {messages_after} messages ({tokens_before} tokens before{cost_suffix})\x1b[0m"
        ));
    }

    fn stopped_for_budget(&self, spent_usd: f64, budget_usd: f64) {
        self.emit_line(&format!(
            "\x1b[33m⛔ stopped: session cost ${spent_usd:.6} has reached the ${budget_usd:.2} budget\x1b[0m\n\x1b[90m  raise it with --budget, or /budget <amount>, or /budget off\x1b[0m"
        ));
    }

    fn stopped_for_context_limit(&self, estimated_tokens: u64, context_window: u64) {
        self.emit_line(&format!(
            "\x1b[33m⛔ stopped: this request is ~{estimated_tokens} tokens, too large for the active model's {context_window}-token context window\x1b[0m\n\x1b[90m  trim the input, /clear and start fresh, or /model to a bigger-context one\x1b[0m"
        ));
    }
}

/// What to write before a body. Never erases unclosed prose.
fn preamble(cursor: Cursor, starts_new_line: bool) -> &'static str {
    match cursor {
        Cursor::LineStart => "",
        Cursor::AfterPreview => "\r\x1b[2K",
        Cursor::AfterProse if starts_new_line => "\n",
        Cursor::AfterProse => "",
    }
}

/// Pads tool names into a column. `semantic_search` is the longest at 15.
const TOOL_NAME_WIDTH: usize = 15;

/// Terminal width; queried per line so a resize is picked up.
fn width() -> usize {
    terminal_size::terminal_size()
        .map(|(terminal_size::Width(w), _)| w as usize)
        .unwrap_or(100)
        .clamp(40, 200)
}

/// The one argument worth seeing at a glance. Unknown tools fall back to raw JSON.
fn describe_args(name: &str, args: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return args.to_string();
    };
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
    let len = |k: &str| v.get(k).and_then(|x| x.as_array()).map(Vec::len);

    let described = match name {
        "read_file" | "write_file" | "edit_file" | "list_dir" | "create_diagram" | "create_pdf"
        | "create_spreadsheet" => s("path").map(str::to_string),
        // `path` is optional here; absent means the whole workspace.
        "project_map" => Some(s("path").unwrap_or(".").to_string()),
        "search" | "semantic_search" | "web_search" => s("query").map(|q| format!("\"{q}\"")),
        "run_shell" => s("command").map(str::to_string),
        "web_fetch" => s("url").map(str::to_string),
        "read_artifact" => s("handle").map(str::to_string),
        "todo_write" => len("todos").map(|n| format!("{n} item{}", plural(n))),
        "read_program" => len("operations").map(|n| format!("{n} operation{}", plural(n))),
        _ => None,
    };
    described.unwrap_or_else(|| args.to_string())
}

/// What the call achieved. Every number is derived from `result`, never guessed.
fn describe_result(name: &str, result: &str) -> String {
    match name {
        // `slice_file` appends "[lines A-B of C]" only for a ranged read.
        "read_file" | "read_artifact" => match trailing_note(result) {
            Some(note) => note,
            None => format!(
                "{} line{} · {}",
                result.lines().count(),
                plural(result.lines().count()),
                human_bytes(result.len())
            ),
        },
        "list_dir" => {
            if result.trim() == "(empty)" {
                "empty".to_string()
            } else {
                let n = result.lines().filter(|l| !l.trim().is_empty()).count();
                format!("{n} entr{}", if n == 1 { "y" } else { "ies" })
            }
        }
        _ => result.lines().next().unwrap_or_default().to_string(),
    }
}

/// The `[...]` note `slice_file` appends to a ranged read, if present.
fn trailing_note(result: &str) -> Option<String> {
    let tail = result.trim_end();
    let start = tail.rfind('[')?;
    tail.ends_with(']')
        .then(|| tail[start + 1..tail.len() - 1].to_string())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn human_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_argument_renders_as_the_bare_path_not_its_json() {
        assert_eq!(
            describe_args("read_file", r#"{"path":"README.md"}"#),
            "README.md"
        );
    }

    #[test]
    fn a_query_argument_is_quoted_so_it_reads_as_a_phrase() {
        assert_eq!(
            describe_args("search", r#"{"query":"fn main","path":"src"}"#),
            "\"fn main\""
        );
    }

    #[test]
    fn project_map_without_a_path_means_the_whole_workspace() {
        assert_eq!(describe_args("project_map", "{}"), ".");
    }

    #[test]
    fn list_arguments_are_summarized_by_count_with_correct_plurals() {
        assert_eq!(
            describe_args("todo_write", r#"{"todos":[{"content":"a"}]}"#),
            "1 item"
        );
        assert_eq!(
            describe_args(
                "todo_write",
                r#"{"todos":[{"content":"a"},{"content":"b"}]}"#
            ),
            "2 items"
        );
    }

    /// Guards a future tool against rendering as a blank line.
    #[test]
    fn an_unknown_tool_falls_back_to_its_raw_arguments() {
        let args = r#"{"whatever":1}"#;
        assert_eq!(describe_args("some_future_tool", args), args);
        assert_eq!(
            describe_args("read_file", "not json at all"),
            "not json at all"
        );
    }

    #[test]
    fn a_whole_file_read_is_summarized_by_line_count_and_size() {
        let summary = describe_result("read_file", "one\ntwo\nthree");
        assert!(summary.starts_with("3 lines · "), "{summary}");
    }

    /// The note knows the full file length; the slice does not.
    #[test]
    fn a_ranged_read_prefers_the_note_slice_file_appended() {
        let result = "line one\nline two\n\n[lines 1-2 of 412; read on with offset=3]";
        assert_eq!(
            describe_result("read_file", result),
            "lines 1-2 of 412; read on with offset=3"
        );
    }

    #[test]
    fn list_dir_is_summarized_by_entry_count() {
        assert_eq!(describe_result("list_dir", "a.rs\nb.rs\nsrc/"), "3 entries");
        assert_eq!(describe_result("list_dir", "only.rs"), "1 entry");
        assert_eq!(describe_result("list_dir", "(empty)"), "empty");
    }

    #[test]
    fn an_unsummarized_tool_shows_its_first_line_only() {
        assert_eq!(
            describe_result("write_file", "wrote 12 lines\ntrailing detail"),
            "wrote 12 lines"
        );
    }

    #[test]
    fn one_line_collapses_newlines_and_marks_truncation() {
        assert_eq!(one_line("a\nb\nc", 40), "a b c");
        assert_eq!(one_line("abcdef", 3), "abc…");
    }

    #[test]
    fn width_stays_inside_bounds_even_with_no_terminal_attached() {
        let w = width();
        assert!((40..=200).contains(&w), "width() returned {w}");
    }

    const ERASE: &str = "\r\x1b[2K";

    /// Erasing here truncated the model's answer mid-word.
    #[test]
    fn unclosed_prose_is_never_erased_by_what_follows_it() {
        assert_eq!(preamble(Cursor::AfterProse, true), "\n");
        assert_ne!(
            preamble(Cursor::AfterProse, true),
            ERASE,
            "erasing here destroys the last line of the model's answer"
        );
    }

    #[test]
    fn streamed_prose_continues_its_line_rather_than_breaking_it() {
        assert_eq!(preamble(Cursor::AfterProse, false), "");
    }

    #[test]
    fn the_preview_is_erased_in_place_by_anything_that_follows() {
        assert_eq!(preamble(Cursor::AfterPreview, true), ERASE);
        assert_eq!(preamble(Cursor::AfterPreview, false), ERASE);
    }

    #[test]
    fn a_fresh_row_needs_no_preamble_at_all() {
        assert_eq!(preamble(Cursor::LineStart, true), "");
        assert_eq!(preamble(Cursor::LineStart, false), "");
    }
}
