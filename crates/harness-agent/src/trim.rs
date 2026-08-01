//! Aging out old tool results, so a long session stops re-sending the same
//! huge blob on every turn.
//!
//! Measured across real saved sessions, 92-96% of a transcript is tool
//! results, and a single `project_map` was routinely 42% of the whole
//! context -- one was 18,177 tokens, re-sent on ~5 later model calls for
//! ~91k tokens billed. Nothing removed it: the only existing mechanism was
//! compaction at 75% of the window, which is a *billed model call* and folds
//! the conversation itself away too.
//!
//! This runs first and costs nothing: no model call, no summarization, just
//! replacing the body of a large, old tool result with a line saying what it
//! was and how to get it back. The message itself stays -- same role, same
//! `tool_call_id` -- so the assistant/tool pairing every OpenAI-dialect API
//! validates on is untouched.

use harness_types::{Message, Role};

/// Absolute size above which aging kicks in, and the size it aims to get
/// back down to.
///
/// Absolute, not a share of the context window, because the window is the
/// wrong instrument here. The default model's window is 1,048,576 tokens,
/// so *any* percentage trigger is unreachable in practice -- measured
/// against real saved sessions, the largest transcript was ~43k tokens,
/// under 5% of it. A window-relative rule looks reasonable and then simply
/// never fires. (`compaction_threshold_percent`'s 75% has exactly this
/// problem: 786k tokens on the default model.)
///
/// What actually costs money and latency is re-transmission: a 14k-token
/// result is re-sent on every later turn no matter how much window is free.
/// So the trigger is tied to transcript size, which is what drives that.
const TRIM_ABOVE_TOKENS: u64 = 25_000;
const TRIM_TARGET_TOKENS: u64 = 15_000;

/// Secondary, window-relative trigger, kept for genuinely small-window
/// models where the absolute figures above would be too permissive.
/// Whichever limit is hit first wins.
const TRIM_ABOVE_PERCENT: u64 = 45;
const TRIM_TARGET_PERCENT: u64 = 30;

/// Most recent messages never touched, whatever the pressure. The model is
/// usually mid-thought about these, and eliding one it just received would
/// read as the tool having returned nothing.
const KEEP_RECENT: usize = 8;

/// Results below this are not worth the round trip of re-fetching.
const MIN_TRIM_CHARS: usize = 2_000;

/// Marker for an already-elided result, so repeated passes are idempotent
/// and never re-trim (or double-count) the same message.
const ELIDED_PREFIX: &str = "[elided:";

pub struct TrimReport {
    pub results_elided: usize,
    pub tokens_saved: u64,
}

/// Replace the body of large, old tool results until the estimate is back
/// under target. Returns `None` when nothing needed doing.
///
/// `messages[0]` (the system prompt) is never a tool result, so it is safe
/// by construction rather than by special case.
pub fn trim_old_tool_results(
    messages: &mut [Message],
    estimated_tokens: u64,
    context_window: u64,
) -> Option<TrimReport> {
    if estimated_tokens == 0 {
        return None;
    }
    // Whichever limit bites first: the absolute one on a big-window model,
    // the window-relative one on a small-window model.
    let (trigger, target) = if context_window == 0 {
        (TRIM_ABOVE_TOKENS, TRIM_TARGET_TOKENS)
    } else {
        (
            TRIM_ABOVE_TOKENS.min(context_window.saturating_mul(TRIM_ABOVE_PERCENT) / 100),
            TRIM_TARGET_TOKENS.min(context_window.saturating_mul(TRIM_TARGET_PERCENT) / 100),
        )
    };
    if estimated_tokens < trigger {
        return None;
    }
    let cutoff = messages.len().saturating_sub(KEEP_RECENT);

    let mut running = estimated_tokens;
    let mut elided = 0usize;
    let mut saved = 0u64;

    // Oldest first: the earliest results are the ones most likely to have
    // been superseded by later work.
    for m in messages[..cutoff].iter_mut() {
        if running <= target {
            break;
        }
        if m.role != Role::Tool
            || m.content.len() < MIN_TRIM_CHARS
            || m.content.starts_with(ELIDED_PREFIX)
        {
            continue;
        }

        let before = crate::tokens::estimate_message_tokens(m);
        let name = m.name.clone().unwrap_or_else(|| "tool".to_string());
        m.content = format!(
            "{ELIDED_PREFIX} ~{before} tokens of `{name}` output from earlier in this session, \
             dropped to free context. Run it again if you still need it.]"
        );
        let after = crate::tokens::estimate_message_tokens(m);

        let freed = before.saturating_sub(after);
        running = running.saturating_sub(freed);
        saved += freed;
        elided += 1;
    }

    (elided > 0).then_some(TrimReport {
        results_elided: elided,
        tokens_saved: saved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_msg(name: &str, chars: usize) -> Message {
        Message::tool_result("call-1", name, "x".repeat(chars))
    }

    /// Enough history that the oldest entries sit outside KEEP_RECENT.
    fn session(big_results: usize, chars: usize) -> Vec<Message> {
        let mut m = vec![Message::system("sys"), Message::user("go")];
        for _ in 0..big_results {
            m.push(tool_msg("project_map", chars));
        }
        for _ in 0..KEEP_RECENT {
            m.push(Message::assistant("recent"));
        }
        m
    }

    #[test]
    fn a_small_transcript_is_left_completely_alone() {
        let mut m = session(1, 8_000);
        let before = crate::tokens::estimate_tokens(&m);
        // Plenty of headroom: 8k chars against a 1M window.
        assert!(trim_old_tool_results(&mut m, before, 1_000_000).is_none());
        assert_eq!(crate::tokens::estimate_tokens(&m), before);
    }

    #[test]
    fn a_large_old_result_is_elided_and_really_shrinks_the_estimate() {
        let mut m = session(1, 60_000);
        let before = crate::tokens::estimate_tokens(&m);
        let report = trim_old_tool_results(&mut m, before, 24_000).expect("should trim");
        let after = crate::tokens::estimate_tokens(&m);

        assert_eq!(report.results_elided, 1);
        assert!(after < before / 2, "{before} -> {after}");
        assert!(report.tokens_saved > 0);
        // The message survives as a valid tool result -- only its body changed.
        let elided = m.iter().find(|x| x.role == Role::Tool).unwrap();
        assert_eq!(elided.tool_call_id.as_deref(), Some("call-1"));
        assert!(elided.content.starts_with(ELIDED_PREFIX), "{}", elided.content);
        assert!(elided.content.contains("project_map"), "should name the tool");
    }

    /// The regression this module's trigger was redesigned around: the
    /// default model's window is 1,048,576 tokens, so any percentage-based
    /// rule is unreachable -- 45% of it is ~472k, and the largest real
    /// session measured was ~43k. A window-relative trigger alone looks
    /// sensible and silently never fires.
    #[test]
    fn a_huge_context_window_does_not_disable_trimming() {
        let mut m = session(3, 40_000); // ~36k tokens, nowhere near 1M
        let before = crate::tokens::estimate_tokens(&m);
        assert!(
            before * 100 < 1_048_576 * TRIM_ABOVE_PERCENT,
            "fixture must be well under the percentage trigger, else it proves nothing"
        );

        let report = trim_old_tool_results(&mut m, before, 1_048_576)
            .expect("must still trim on a 1M-token window");
        assert!(report.tokens_saved > 0);
        assert!(crate::tokens::estimate_tokens(&m) < before);
    }

    #[test]
    fn the_most_recent_results_are_never_touched() {
        // Every tool result sits inside the keep-recent window.
        let mut m = vec![Message::system("sys"), Message::user("go")];
        for _ in 0..KEEP_RECENT {
            m.push(tool_msg("read_file", 60_000));
        }
        let before = crate::tokens::estimate_tokens(&m);
        assert!(trim_old_tool_results(&mut m, before, 24_000).is_none());
        assert!(m.iter().all(|x| !x.content.starts_with(ELIDED_PREFIX)));
    }

    #[test]
    fn trimming_stops_once_back_under_target_instead_of_eliding_everything() {
        // ~12.1k tokens per result, so 6 is ~72.8k. Against a 120k window
        // that is over the 45% trigger and needs ~4 elided to get back under
        // the 30% target -- leaving the newest ones intact, which is the
        // behaviour being pinned.
        let mut m = session(6, 40_000);
        let before = crate::tokens::estimate_tokens(&m);
        let report = trim_old_tool_results(&mut m, before, 120_000).expect("should trim");
        let remaining = m
            .iter()
            .filter(|x| x.role == Role::Tool && !x.content.starts_with(ELIDED_PREFIX))
            .count();
        assert!(
            remaining > 0,
            "should have stopped at the target, not elided all 6"
        );
        assert!(report.results_elided < 6, "elided {}", report.results_elided);
    }

    #[test]
    fn running_twice_is_idempotent() {
        let mut m = session(1, 60_000);
        let before = crate::tokens::estimate_tokens(&m);
        trim_old_tool_results(&mut m, before, 24_000).expect("first pass trims");
        let after_first = crate::tokens::estimate_tokens(&m);

        // A second pass must not re-wrap the placeholder in another one.
        let second = trim_old_tool_results(&mut m, after_first, 24_000);
        assert!(second.is_none(), "nothing left worth eliding");
        assert_eq!(crate::tokens::estimate_tokens(&m), after_first);
    }

    #[test]
    fn small_results_are_not_worth_eliding() {
        let mut m = session(4, MIN_TRIM_CHARS - 1);
        let before = crate::tokens::estimate_tokens(&m);
        assert!(trim_old_tool_results(&mut m, before, 4_000).is_none());
    }
}
