use harness_types::{Message, Role};

const TRIM_ABOVE_PERCENT: u64 = 50;
const TRIM_TARGET_PERCENT: u64 = 30;
const TRIM_ABOVE_CEILING_TOKENS: u64 = 200_000;
const TRIM_TARGET_CEILING_TOKENS: u64 = 120_000;
#[cfg(test)]
const MAX_SINGLE_RESULT_SHARE_OF_TARGET: f64 = 0.25;

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

pub fn trim_old_tool_results(
    messages: &mut [Message],
    estimated_tokens: u64,
    context_window: u64,
    current_request_start: usize,
) -> Option<TrimReport> {
    if estimated_tokens == 0 {
        return None;
    }
    // Whichever limit bites first: the ceiling on a big-window model, the
    // window-relative share on a small-window one.
    let (trigger, target) = if context_window == 0 {
        (TRIM_ABOVE_CEILING_TOKENS, TRIM_TARGET_CEILING_TOKENS)
    } else {
        (
            TRIM_ABOVE_CEILING_TOKENS.min(context_window.saturating_mul(TRIM_ABOVE_PERCENT) / 100),
            TRIM_TARGET_CEILING_TOKENS
                .min(context_window.saturating_mul(TRIM_TARGET_PERCENT) / 100),
        )
    };
    if estimated_tokens < trigger {
        return None;
    }
    let cutoff = messages.len().saturating_sub(KEEP_RECENT);
    let boundary = current_request_start.min(cutoff);

    let mut state = Elider {
        running: estimated_tokens,
        target,
        elided: 0,
        saved: 0,
    };

    // Previous requests first. Their results are the ones most likely to
    // have been superseded, and dropping them cannot pull the floor out
    // from under the task in flight.
    let (earlier, current) = messages[..cutoff].split_at_mut(boundary);
    state.elide_span(earlier);
    state.elide_span(current);

    (state.elided > 0).then_some(TrimReport {
        results_elided: state.elided,
        tokens_saved: state.saved,
    })
}

/// Running totals for one trim pass, so the two spans above share a budget
/// instead of each getting a full one.
struct Elider {
    running: u64,
    target: u64,
    elided: usize,
    saved: u64,
}

impl Elider {
    /// Replaces large tool-result bodies in `span`, oldest first, until the
    /// running estimate is back under target.
    fn elide_span(&mut self, span: &mut [Message]) {
        for m in span.iter_mut() {
            if self.running <= self.target {
                return;
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
                "{ELIDED_PREFIX} ~{before} tokens of `{name}` output from an earlier request in \
                 this session, dropped to free context. It is most likely not needed for the \
                 current request. If it is, fetch only the specific part you need rather than \
                 repeating the whole call.]"
            );
            let after = crate::tokens::estimate_message_tokens(m);

            let freed = before.saturating_sub(after);
            self.running = self.running.saturating_sub(freed);
            self.saved += freed;
            self.elided += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_msg(name: &str, chars: usize) -> Message {
        Message::tool_result("call-1", name, "x".repeat(chars))
    }

    fn trim(m: &mut [Message], estimated: u64, window: u64) -> Option<TrimReport> {
        trim_old_tool_results(m, estimated, window, 0)
    }

    fn tokens_of(chars: usize) -> u64 {
        crate::tokens::estimate_message_tokens(&tool_msg("read_file", chars))
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
        assert!(trim(&mut m, before, 1_000_000).is_none());
        assert_eq!(crate::tokens::estimate_tokens(&m), before);
    }

    #[test]
    fn a_large_old_result_is_elided_and_really_shrinks_the_estimate() {
        let mut m = session(1, 60_000);
        let before = crate::tokens::estimate_tokens(&m);
        let report = trim(&mut m, before, 24_000).expect("should trim");
        let after = crate::tokens::estimate_tokens(&m);

        assert_eq!(report.results_elided, 1);
        assert!(after < before / 2, "{before} -> {after}");
        assert!(report.tokens_saved > 0);
        // The message survives as a valid tool result -- only its body changed.
        let elided = m.iter().find(|x| x.role == Role::Tool).unwrap();
        assert_eq!(elided.tool_call_id.as_deref(), Some("call-1"));
        assert!(
            elided.content.starts_with(ELIDED_PREFIX),
            "{}",
            elided.content
        );
        assert!(
            elided.content.contains("project_map"),
            "should name the tool"
        );
    }

    #[test]
    fn an_ordinary_working_set_on_a_huge_window_is_left_alone() {
        let mut m = session(3, 40_000); // ~36k tokens against 1M
        let before = crate::tokens::estimate_tokens(&m);
        assert!(
            trim(&mut m, before, 1_048_576).is_none(),
            "a 36k transcript on a 1M window must not be trimmed"
        );
        assert!(m.iter().all(|x| !x.content.starts_with(ELIDED_PREFIX)));
    }

    #[test]
    fn the_two_files_that_broke_a_real_task_now_fit_together() {
        let mut m = vec![Message::system("sys"), Message::user("go")];
        m.push(tool_msg("read_file", 58_800)); // agent.rs
        m.push(tool_msg("read_file", 74_618)); // main.rs
        for _ in 0..KEEP_RECENT {
            m.push(Message::assistant("working"));
        }
        let before = crate::tokens::estimate_tokens(&m);
        assert!(
            before > 33_000,
            "fixture should be the ~33k that used to be untenable, got {before}"
        );
        assert!(
            trim(&mut m, before, 1_048_576).is_none(),
            "both files must survive together: this is the task that failed"
        );
    }

    #[test]
    fn one_maximal_read_cannot_amount_to_the_whole_budget() {
        let max_result_tokens = tokens_of(harness_tools::MAX_READ_BYTES) as f64;
        let share = max_result_tokens / TRIM_TARGET_CEILING_TOKENS as f64;
        assert!(
            share <= MAX_SINGLE_RESULT_SHARE_OF_TARGET,
            "one read_file is {share:.0e} of the trim target ({max_result_tokens} tokens vs \
             {TRIM_TARGET_CEILING_TOKENS}); raise the ceiling or lower MAX_READ_BYTES"
        );
    }

    /// Trim must stay below compaction, which is the billed mechanism, and
    /// below the send guard that stops a request going out oversized.
    #[test]
    fn trimming_happens_before_compaction_and_well_before_the_send_guard() {
        for window in [32_000u64, 128_000, 1_048_576] {
            let trigger = TRIM_ABOVE_CEILING_TOKENS.min(window * TRIM_ABOVE_PERCENT / 100);
            assert!(
                trigger < window * 75 / 100,
                "trim must precede compaction on a {window}-token window"
            );
            assert!(trigger < window * 95 / 100);
        }
    }

    /// What the current request has read is protected while anything older
    /// is still available to give -- taking it is what makes the agent read
    /// the same file again.
    #[test]
    fn an_older_request_is_evicted_before_anything_the_current_one_read() {
        let mut m = vec![Message::system("sys"), Message::user("first request")];
        m.push(tool_msg("project_map", 40_000)); // belongs to the old request
        let boundary = m.len();
        m.push(Message::user("second request"));
        m.push(tool_msg("read_file", 40_000)); // the current request's own read
        for _ in 0..KEEP_RECENT {
            m.push(Message::assistant("working"));
        }

        // Sized so that giving up the older result alone is enough to get
        // back under target: what is being pinned is which one goes first,
        // not how many go.
        let before = crate::tokens::estimate_tokens(&m);
        trim_old_tool_results(&mut m, before, 44_000, boundary).expect("should trim");

        let elided: Vec<&str> = m
            .iter()
            .filter(|x| x.content.starts_with(ELIDED_PREFIX))
            .map(|x| x.name.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            elided,
            vec!["project_map"],
            "the current request's own read must outlive the previous request's"
        );
    }

    /// ...but protection is a preference, not a wall. A single request that
    /// genuinely outgrows the budget still gets trimmed rather than being
    /// allowed to run into the send guard.
    #[test]
    fn a_single_oversized_request_is_still_trimmed_when_nothing_older_exists() {
        let mut m = vec![Message::system("sys"), Message::user("go")];
        let boundary = 1; // everything from the user turn on is this request
        for _ in 0..6 {
            m.push(tool_msg("read_file", 40_000));
        }
        for _ in 0..KEEP_RECENT {
            m.push(Message::assistant("working"));
        }
        let before = crate::tokens::estimate_tokens(&m);
        let report =
            trim_old_tool_results(&mut m, before, 120_000, boundary).expect("must still trim");
        assert!(report.results_elided > 0);
    }

    /// The elision note is read by the model, and the previous wording
    /// ("Run it again if you still need it") was followed literally enough
    /// to cost a 60-turn run.
    #[test]
    fn the_elision_note_does_not_tell_the_model_to_re_run_the_call() {
        let mut m = session(1, 60_000);
        let before = crate::tokens::estimate_tokens(&m);
        trim(&mut m, before, 24_000).expect("should trim");
        let note = &m.iter().find(|x| x.role == Role::Tool).unwrap().content;

        assert!(
            !note.to_lowercase().contains("run it again"),
            "must not instruct a re-run: {note}"
        );
        assert!(note.contains("earlier request"), "should place it in time");
        assert!(
            note.contains("only the specific part"),
            "should point at a narrower fetch: {note}"
        );
    }

    #[test]
    fn the_most_recent_results_are_never_touched() {
        // Every tool result sits inside the keep-recent window.
        let mut m = vec![Message::system("sys"), Message::user("go")];
        for _ in 0..KEEP_RECENT {
            m.push(tool_msg("read_file", 60_000));
        }
        let before = crate::tokens::estimate_tokens(&m);
        assert!(trim(&mut m, before, 24_000).is_none());
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
        let report = trim(&mut m, before, 120_000).expect("should trim");
        let remaining = m
            .iter()
            .filter(|x| x.role == Role::Tool && !x.content.starts_with(ELIDED_PREFIX))
            .count();
        assert!(
            remaining > 0,
            "should have stopped at the target, not elided all 6"
        );
        assert!(
            report.results_elided < 6,
            "elided {}",
            report.results_elided
        );
    }

    #[test]
    fn running_twice_is_idempotent() {
        let mut m = session(1, 60_000);
        let before = crate::tokens::estimate_tokens(&m);
        trim(&mut m, before, 24_000).expect("first pass trims");
        let after_first = crate::tokens::estimate_tokens(&m);

        // A second pass must not re-wrap the placeholder in another one.
        let second = trim(&mut m, after_first, 24_000);
        assert!(second.is_none(), "nothing left worth eliding");
        assert_eq!(crate::tokens::estimate_tokens(&m), after_first);
    }

    #[test]
    fn small_results_are_not_worth_eliding() {
        let mut m = session(4, MIN_TRIM_CHARS - 1);
        let before = crate::tokens::estimate_tokens(&m);
        assert!(trim(&mut m, before, 4_000).is_none());
    }
}
